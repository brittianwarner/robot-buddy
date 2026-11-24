#!/usr/bin/env python3
"""Drop-in ALSA `arecord` shim for simulated ARM64 Pip microphone.

Matches apps/robot/src/listen/io.rs invocation:
  arecord -q -D <device> -t raw -f S16_LE -r 16000 -c <channels>
Survives listen/io.rs env_clear() by receiving --sock from the launcher wrapper in
<run_dir>/bin/arecord. Streams 20 ms 16 kHz S16_LE frames from AudioBridge to stdout.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import struct
import sys


def _split_wrapper_args(raw_argv: list[str]) -> tuple[str, list[str]]:
    if len(raw_argv) >= 3 and raw_argv[0] == "--sock":
        sock_path = raw_argv[1]
        rest = raw_argv[2:]
        if rest and rest[0] == "--":
            rest = rest[1:]
        return sock_path, rest
    sock_path = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "ipc", "mic.sock"))
    return sock_path, raw_argv


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("mic IPC closed")
        buf.extend(chunk)
    return bytes(buf)


def main() -> int:
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    sock_path, args = _split_wrapper_args(sys.argv[1:])

    device = "default"
    rate = 16000
    channels = 1

    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "-D" and i + 1 < len(args):
            device = args[i + 1]
            i += 2
        elif arg == "-r" and i + 1 < len(args):
            rate = int(args[i + 1])
            i += 2
        elif arg == "-c" and i + 1 < len(args):
            channels = int(args[i + 1])
            i += 2
        elif arg in ("-t", "-f") and i + 1 < len(args):
            i += 2
        else:
            i += 1

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(sock_path)
    except OSError as exc:
        sys.stderr.write(f"arecord shim: cannot connect to {sock_path}: {exc}\n")
        return 1

    hello = {
        "pid": os.getpid(),
        "argv": args,
        "device": device,
        "rate": rate,
        "channels": channels,
    }
    sock.sendall((json.dumps(hello, separators=(",", ":")) + "\n").encode("utf-8"))

    out = sys.stdout.buffer
    try:
        while True:
            hdr = _recv_exact(sock, 5)
            opcode, length = struct.unpack("!BI", hdr)
            payload = _recv_exact(sock, length) if length > 0 else b""
            if opcode == 1:
                out.write(payload)
                out.flush()
            elif opcode == 2:
                return payload[0] if payload else 1
    except (EOFError, BrokenPipeError):
        return 0
    finally:
        try:
            sock.close()
        except OSError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
