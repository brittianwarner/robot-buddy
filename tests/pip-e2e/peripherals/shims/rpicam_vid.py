#!/usr/bin/env python3
"""Drop-in rpicam-vid shim for simulated ARM64 Pip camera.

Validates exact rpicam-vid arguments used by apps/robot/src/vision/mod.rs:
  --nopreview --timeout 0 --codec yuv420 --width 384 --height 288 --framerate 15
  [--rotation 180] [--frames N] --output -
Streams packed I420 frames from <run_dir>/ipc/camera.sock to stdout.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import struct
import sys

OP_FRAME = 1
OP_TRUNCATED_EXIT = 2
OP_EXIT = 3


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise EOFError("camera IPC closed")
        buf.extend(chunk)
    return bytes(buf)


def _split_wrapper_args(raw_argv: list[str]) -> tuple[str, list[str]]:
    if len(raw_argv) >= 3 and raw_argv[0] == "--sock":
        sock_path = raw_argv[1]
        rest = raw_argv[2:]
        if rest and rest[0] == "--":
            rest = rest[1:]
        return sock_path, rest
    sock_path = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "ipc", "camera.sock"))
    return sock_path, raw_argv


def main() -> int:
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    sock_path, args = _split_wrapper_args(sys.argv[1:])

    width = 384
    height = 288
    framerate = 15.0
    rotation = 0
    frames = 0
    codec = ""
    output = ""

    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "--nopreview":
            i += 1
        elif arg in ("--timeout", "-t") and i + 1 < len(args):
            i += 2
        elif arg == "--codec" and i + 1 < len(args):
            codec = args[i + 1]
            i += 2
        elif arg == "--width" and i + 1 < len(args):
            width = int(args[i + 1])
            i += 2
        elif arg == "--height" and i + 1 < len(args):
            height = int(args[i + 1])
            i += 2
        elif arg == "--framerate" and i + 1 < len(args):
            framerate = float(args[i + 1])
            i += 2
        elif arg == "--rotation" and i + 1 < len(args):
            rotation = int(args[i + 1])
            i += 2
        elif arg == "--frames" and i + 1 < len(args):
            frames = int(args[i + 1])
            i += 2
        elif arg in ("--output", "-o") and i + 1 < len(args):
            output = args[i + 1]
            i += 2
        else:
            sys.stderr.write(f"rpicam-vid shim: unsupported arg {arg!r}\n")
            return 2

    if codec != "yuv420" or width != 384 or height != 288 or output != "-":
        sys.stderr.write(
            f"rpicam-vid shim: expected --codec yuv420 --width 384 --height 288 --output -, "
            f"got codec={codec!r} {width}x{height} output={output!r}\n"
        )
        return 2

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(sock_path)
    except OSError as exc:
        sys.stderr.write(f"rpicam-vid shim: cannot connect to {sock_path}: {exc}\n")
        return 1

    hello = {
        "pid": os.getpid(),
        "argv": args,
        "width": width,
        "height": height,
        "framerate": framerate,
        "rotation": rotation,
        "frames": frames,
    }
    sock.sendall((json.dumps(hello) + "\n").encode("utf-8"))

    out = sys.stdout.buffer
    try:
        while True:
            hdr = _recv_exact(sock, 5)
            opcode, length = struct.unpack("!BI", hdr)
            payload = _recv_exact(sock, length) if length > 0 else b""
            if opcode == OP_FRAME:
                out.write(payload)
                out.flush()
            elif opcode == OP_TRUNCATED_EXIT:
                if payload:
                    out.write(payload)
                    out.flush()
                return 1
            elif opcode == OP_EXIT:
                return payload[0] if payload else 0
    except (EOFError, BrokenPipeError):
        return 0
    finally:
        try:
            sock.close()
        except OSError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
