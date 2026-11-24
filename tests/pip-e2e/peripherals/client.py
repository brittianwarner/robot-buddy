"""Python client for ARM64 virtual Pip peripheral supervisor IPC (Unix JSON-RPC)."""

from __future__ import annotations

import json
import socket
from pathlib import Path
from typing import Any


class PeripheralRpcError(RuntimeError):
    pass


class PeripheralsClient:
    """Thread-safe newline-delimited JSON-RPC client over <run_dir>/control.sock."""

    def __init__(self, sock_path: str | Path, timeout: float = 35.0) -> None:
        self.sock_path = str(sock_path)
        self.timeout = float(timeout)
        self._seq = 0
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.settimeout(self.timeout)
        self._sock.connect(self.sock_path)
        self._buf = bytearray()

    def close(self) -> None:
        try:
            self._sock.close()
        except OSError:
            pass

    def __enter__(self) -> "PeripheralsClient":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        self._seq += 1
        req_id = self._seq
        payload = json.dumps(
            {"id": req_id, "method": method, "params": params or {}},
            separators=(",", ":"),
        ) + "\n"
        self._sock.sendall(payload.encode("utf-8"))
        while True:
            nl = self._buf.find(b"\n")
            if nl >= 0:
                line = bytes(self._buf[:nl])
                del self._buf[: nl + 1]
                resp = json.loads(line.decode("utf-8"))
                if not resp.get("ok"):
                    raise PeripheralRpcError(resp.get("error") or f"RPC failed: {method}")
                return resp.get("result")
            chunk = self._sock.recv(65536)
            if not chunk:
                raise EOFError("peripherals control socket closed")
            self._buf.extend(chunk)
