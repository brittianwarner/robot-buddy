#!/usr/bin/env python3
"""Drop-in ALSA `aplay` shim for simulated ARM64 Pip speaker.

Matches apps/robot/src/audio.rs invocation:
  aplay -q -t raw -f S16_LE --buffer-time=<us> -r <rate> -c <channels> [-D <device>]
Consumes raw S16_LE PCM from stdin, reports chunk timing/byte counts and PCM (when
echo or capture is enabled) to <run_dir>/ipc/speaker.sock, paces drainage at
realtime sample rate so barge-in SIGKILL interrupts cleanly, and exits 0 after drain.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import sys
import time


def _now_ms() -> float:
    return round(time.time() * 1000.0, 3)


def _split_wrapper_args(raw_argv: list[str]) -> tuple[str, list[str]]:
    if len(raw_argv) >= 3 and raw_argv[0] == "--sock":
        sock_path = raw_argv[1]
        rest = raw_argv[2:]
        if rest and rest[0] == "--":
            rest = rest[1:]
        return sock_path, rest
    sock_path = os.path.abspath(
        os.path.join(os.path.dirname(__file__), "..", "ipc", "speaker.sock")
    )
    return sock_path, raw_argv


def _recv_line(sock: socket.socket) -> bytes:
    buf = bytearray()
    while len(buf) < 65536:
        ch = sock.recv(1)
        if not ch or ch == b"\n":
            break
        buf.extend(ch)
    return bytes(buf)


def main() -> int:
    sock_path, args = _split_wrapper_args(sys.argv[1:])

    rate = 24000
    channels = 1
    buffer_us = 120000
    device = None

    i = 0
    while i < len(args):
        arg = args[i]
        if arg == "-r" and i + 1 < len(args):
            rate = int(args[i + 1])
            i += 2
        elif arg == "-c" and i + 1 < len(args):
            channels = int(args[i + 1])
            i += 2
        elif arg == "-D" and i + 1 < len(args):
            device = args[i + 1]
            i += 2
        elif arg.startswith("--buffer-time="):
            buffer_us = int(arg.split("=", 1)[1])
            i += 1
        elif arg in ("-t", "-f") and i + 1 < len(args):
            i += 2
        else:
            i += 1

    bytes_per_sec = max(1, rate * channels * 2)

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(sock_path)
    except OSError as exc:
        sys.stderr.write(f"aplay shim: cannot connect to {sock_path}: {exc}\n")
        return 1

    def send_json(obj: dict) -> None:
        sock.sendall((json.dumps(obj, separators=(",", ":")) + "\n").encode("utf-8"))

    total = 0

    def on_term(_sig: int, _frame: object) -> None:
        try:
            send_json({"type": "term", "ts_ms": _now_ms(), "total": total, "exit_code": 143})
        except OSError:
            pass
        os._exit(143)

    signal.signal(signal.SIGTERM, on_term)

    send_json(
        {
            "type": "start",
            "pid": os.getpid(),
            "args": args,
            "rate": rate,
            "channels": channels,
            "buffer_us": buffer_us,
            "device": device,
            "started_ms": _now_ms(),
        }
    )

    cfg_raw = _recv_line(sock)
    cfg = json.loads(cfg_raw.decode("utf-8")) if cfg_raw else {}
    pace_realtime = bool(cfg.get("pace_realtime", True))
    stall_after_bytes = cfg.get("stall_after_bytes")
    exit_early_code = cfg.get("exit_early_code")
    send_pcm = bool(cfg.get("send_pcm", False))

    if exit_early_code is not None:
        send_json({"type": "end", "ts_ms": _now_ms(), "total": 0, "exit_code": int(exit_early_code)})
        sock.close()
        return int(exit_early_code)

    stdin_fd = sys.stdin.fileno()
    play_clock = 0.0
    max_lead_s = max(0.15, (buffer_us / 1_000_000.0) * 2.0)

    try:
        while True:
            chunk = os.read(stdin_fd, 4096)
            if not chunk:
                break
            now_mono = time.monotonic()
            total += len(chunk)
            if stall_after_bytes is not None and total >= int(stall_after_bytes):
                # Hold open without reading further to trigger audio.rs WRITE_DEADLINE.
                while True:
                    time.sleep(1.0)
            hdr = {
                "type": "chunk",
                "ts_ms": _now_ms(),
                "n": len(chunk),
                "b": chunk[0],
                "has_pcm": send_pcm,
            }
            send_json(hdr)
            if send_pcm:
                sock.sendall(chunk)
            if pace_realtime:
                play_clock = max(play_clock, now_mono) + (len(chunk) / bytes_per_sec)
                # Backpressure once buffered beyond virtual ALSA buffer, keeping steady progress.
                lead = play_clock - now_mono
                if lead > max_lead_s:
                    time.sleep(min(lead - max_lead_s, 0.2))

        send_json({"type": "eof", "ts_ms": _now_ms(), "total": total})
        if pace_realtime and play_clock > 0.0:
            rem = play_clock - time.monotonic()
            if rem > 0:
                time.sleep(min(rem, 9.0))
        send_json({"type": "end", "ts_ms": _now_ms(), "total": total, "exit_code": 0})
        return 0
    except (OSError, BrokenPipeError):
        return 1
    finally:
        try:
            sock.close()
        except OSError:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
