#!/usr/bin/env python3
"""Slow real Whisper wrapper for ARM64 Pip E2E & speech qualification.

Matches apps/robot/src/listen/io.rs invocation:
  <BUDDY_LISTEN_WHISPER> -m <model> -l en -t 2 -np -nt -f -
with a complete RIFF WAV on stdin.

Never fabricates or short-circuits transcripts: delegates to the real ARM64
`whisper-cli` binary configured via IPC (`whisper.configure`), while supporting:
- Controllable pre-decode delay (`delay_before_ms`) and post-decode delay (`delay_after_ms`)
- Hold-until-release gate (`hold: true` + `whisper.release`) for deterministic
  slow-whisper / barge-in / state-change cancellation testing
- Linux PR_SET_PDEATHSIG(SIGKILL) on the child `whisper-cli` so when robot-buddy-robot
  cancels an in-flight decode via `kill_on_drop(true)`, child `whisper-cli` dies
  immediately with zero orphan processes and the peripheral daemon observes `cancelled: true`.
"""

from __future__ import annotations

import ctypes
import json
import os
import platform
import signal
import socket
import struct
import subprocess
import sys
import time

PR_SET_PDEATHSIG = 1


def _split_wrapper_args(raw_argv: list[str]) -> tuple[str, list[str]]:
    if len(raw_argv) >= 3 and raw_argv[0] == "--sock":
        sock_path = raw_argv[1]
        rest = raw_argv[2:]
        if rest and rest[0] == "--":
            rest = rest[1:]
        return sock_path, rest
    sock_path = os.path.abspath(
        os.path.join(os.path.dirname(__file__), "..", "ipc", "whisper.sock")
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


def _wav_duration_ms(wav_bytes: bytes) -> float:
    if len(wav_bytes) >= 44 and wav_bytes[:4] == b"RIFF" and wav_bytes[8:12] == b"WAVE":
        sample_rate = struct.unpack_from("<I", wav_bytes, 24)[0] or 16000
        channels = struct.unpack_from("<H", wav_bytes, 22)[0] or 1
        bits = struct.unpack_from("<H", wav_bytes, 34)[0] or 16
        data_bytes = max(0, len(wav_bytes) - 44)
        bytes_per_sec = sample_rate * channels * max(1, bits // 8)
        return round(data_bytes * 1000.0 / bytes_per_sec, 1)
    return 0.0


def _make_pdeathsig_preexec():
    if platform.system() != "Linux":
        return None

    def _set_pdeathsig() -> None:
        try:
            libc = ctypes.CDLL("libc.so.6", use_errno=True)
            libc.prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
        except Exception:
            pass

    return _set_pdeathsig


def main() -> int:
    sock_path, args = _split_wrapper_args(sys.argv[1:])

    wav_data = sys.stdin.buffer.read()
    wav_dur = _wav_duration_ms(wav_data)

    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        sock.connect(sock_path)
    except OSError as exc:
        sys.stderr.write(f"whisper-slow shim: cannot connect to {sock_path}: {exc}\n")
        return 1

    start_msg = {
        "event": "started",
        "pid": os.getpid(),
        "argv": args,
        "wav_bytes": len(wav_data),
        "wav_duration_ms": wav_dur,
    }
    sock.sendall((json.dumps(start_msg, separators=(",", ":")) + "\n").encode("utf-8"))

    cfg_raw = _recv_line(sock)
    if not cfg_raw:
        return 1
    cfg = json.loads(cfg_raw.decode("utf-8"))

    real_bin = str(cfg.get("real_whisper_bin") or "/usr/local/bin/whisper-cli")
    delay_before_ms = max(0, int(cfg.get("delay_before_ms", 0)))
    delay_after_ms = max(0, int(cfg.get("delay_after_ms", 0)))
    hold = bool(cfg.get("hold", False))
    forced_exit = cfg.get("exit_code")

    if delay_before_ms > 0:
        time.sleep(delay_before_ms / 1000.0)

    if hold:
        rel_raw = _recv_line(sock)
        if not rel_raw:
            return 1

    if forced_exit is not None:
        code = int(forced_exit)
        try:
            sock.sendall(
                (json.dumps({"event": "finished", "exit_code": code, "stdout_bytes": 0}) + "\n").encode(
                    "utf-8"
                )
            )
        except OSError:
            pass
        sock.close()
        return code

    if not os.path.isfile(real_bin) or not os.access(real_bin, os.X_OK):
        sys.stderr.write(f"whisper-slow shim: real whisper binary not executable: {real_bin}\n")
        try:
            sock.sendall(
                (json.dumps({"event": "finished", "exit_code": 127, "stdout_bytes": 0}) + "\n").encode(
                    "utf-8"
                )
            )
        except OSError:
            pass
        sock.close()
        return 127

    proc = subprocess.Popen(
        [real_bin, *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        preexec_fn=_make_pdeathsig_preexec(),
    )

    def _forward_term(_sig: int, _frame: object) -> None:
        try:
            proc.kill()
        except OSError:
            pass
        os._exit(143)

    signal.signal(signal.SIGTERM, _forward_term)

    stdout_bytes, stderr_bytes = proc.communicate(input=wav_data)
    code = int(proc.returncode or 0)

    if delay_after_ms > 0:
        time.sleep(delay_after_ms / 1000.0)

    if stdout_bytes:
        sys.stdout.buffer.write(stdout_bytes)
        sys.stdout.buffer.flush()
    if stderr_bytes and code != 0:
        sys.stderr.buffer.write(stderr_bytes[:1024])
        sys.stderr.buffer.flush()

    try:
        sock.sendall(
            (
                json.dumps(
                    {
                        "event": "finished",
                        "exit_code": code,
                        "stdout_bytes": len(stdout_bytes),
                    },
                    separators=(",", ":"),
                )
                + "\n"
            ).encode("utf-8")
        )
    except OSError:
        pass
    finally:
        try:
            sock.close()
        except OSError:
            pass
    return code


if __name__ == "__main__":
    raise SystemExit(main())
