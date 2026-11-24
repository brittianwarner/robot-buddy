"""Audio peripherals (aplay speaker, arecord mic, echo injection, slow real Whisper).

Matches apps/robot/src/audio.rs and apps/robot/src/listen/{io,mod,vad,wake}.rs:
- Fake speaker (`aplay` on PATH): consumes S16_LE PCM (typically 24 kHz mono),
  paces/drains cleanly, detects SIGKILL barge-in immediately via socket EOF,
  records full session telemetry + optional raw PCM captures, and feeds live
  24 kHz -> 16 kHz resampled acoustic echo into the virtual mic when enabled.
- Fake mic (`arecord` on PATH): survives listen/io.rs env_clear(), streams real
  recorded 16 kHz mono S16_LE WAV/PCM fixtures at exact 20 ms (320-sample) cadence
  into the configured channel of `-c <channels>`, mixed with configurable ambient
  noise floor and live speaker echo.
- Slow real Whisper wrapper (`BUDDY_LISTEN_WHISPER`): survives listen/io.rs
  env_clear(), delegates to the REAL ARM64 `whisper-cli` binary (never faking
  transcripts) with controllable pre/post delay or hold-until-release, tracks
  cancellation when kill_on_drop kills the wrapper mid-decode, and uses Linux
  PR_SET_PDEATHSIG so child whisper-cli never orphans.
"""

from __future__ import annotations

import array
import base64
import collections
import json
import math
import os
import random
import socket
import struct
import threading
import time
import wave
from pathlib import Path
from typing import Any

MIC_RATE = 16_000
MIC_FRAME_SAMPLES = 320  # 20 ms at 16 kHz (matches apps/robot/src/listen/vad.rs)
MIC_FRAME_PERIOD_S = MIC_FRAME_SAMPLES / MIC_RATE  # 0.020 s


def _now_ms() -> float:
    return round(time.time() * 1000.0, 3)


def _clamp_i16(val: int) -> int:
    if val > 32767:
        return 32767
    if val < -32768:
        return -32768
    return val


def _resample_i16_mono(samples: array.array, src_rate: int, dst_rate: int) -> array.array:
    if src_rate == dst_rate or len(samples) == 0:
        return samples
    out_len = max(1, int(round(len(samples) * dst_rate / src_rate)))
    ratio = src_rate / dst_rate
    out = array.array("h", [0]) * out_len
    n = len(samples)
    for i in range(out_len):
        pos = i * ratio
        idx = int(pos)
        frac = pos - idx
        s0 = samples[idx] if idx < n else samples[-1]
        s1 = samples[idx + 1] if idx + 1 < n else s0
        out[i] = _clamp_i16(int(round(s0 + (s1 - s0) * frac)))
    return out


def load_pcm_16k_mono(
    path: Path,
    fmt: str | None = None,
    sample_rate: int = MIC_RATE,
    channels: int = 1,
    source_channel: int = 0,
    gain: float = 1.0,
) -> array.array:
    """Load a 16-bit PCM WAV or raw S16_LE PCM file as 16 kHz mono array('h')."""
    p = Path(path)
    if not p.is_file():
        raise FileNotFoundError(f"audio fixture not found: {p}")

    detected_fmt = (fmt or ("wav" if p.suffix.lower() == ".wav" else "s16le")).lower()
    if detected_fmt == "wav":
        with wave.open(str(p), "rb") as wf:
            if wf.getsampwidth() != 2:
                raise ValueError(f"WAV must be 16-bit PCM, got {wf.getsampwidth() * 8}-bit")
            ch = wf.getnchannels()
            rate = wf.getframerate()
            raw = wf.readframes(wf.getnframes())
    elif detected_fmt in ("s16le", "pcm", "raw"):
        ch = int(channels)
        rate = int(sample_rate)
        raw = p.read_bytes()
    else:
        raise ValueError(f"unsupported audio format: {detected_fmt}")

    if len(raw) % (2 * ch) != 0:
        raw = raw[: len(raw) - (len(raw) % (2 * ch))]

    arr = array.array("h")
    arr.frombytes(raw)
    if ch > 1:
        sc = max(0, min(ch - 1, int(source_channel)))
        mono = array.array("h", arr[sc::ch])
    else:
        mono = arr

    if rate != MIC_RATE:
        mono = _resample_i16_mono(mono, rate, MIC_RATE)

    if abs(gain - 1.0) > 1e-6:
        g = float(gain)
        for i in range(len(mono)):
            mono[i] = _clamp_i16(int(round(mono[i] * g)))
    return mono


def _pcm_rms(raw: bytes) -> float:
    if len(raw) < 2:
        return 0.0
    n = len(raw) // 2
    arr = array.array("h")
    arr.frombytes(raw[: n * 2])
    acc = sum(s * s for s in arr)
    return round(math.sqrt(acc / n), 2)


def _recv_exact(conn: socket.socket, n: int) -> bytes:
    buf = bytearray()
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise EOFError("socket closed")
        buf.extend(chunk)
    return bytes(buf)


def _recv_line(conn: socket.socket, max_len: int = 65536) -> bytes:
    buf = bytearray()
    while len(buf) < max_len:
        ch = conn.recv(1)
        if not ch:
            if not buf:
                raise EOFError("socket closed")
            break
        if ch == b"\n":
            break
        buf.extend(ch)
    return bytes(buf)


class AudioBridge:
    """Coordinates fake aplay speaker, fake arecord mic, echo coupling, and slow whisper."""

    def __init__(
        self,
        speaker_sock: Path,
        mic_sock: Path,
        whisper_sock: Path,
        real_whisper_bin: str = "/usr/local/bin/whisper-cli",
        capture_dir: Path | None = None,
    ) -> None:
        self.speaker_sock = Path(speaker_sock)
        self.mic_sock = Path(mic_sock)
        self.whisper_sock = Path(whisper_sock)
        for p in (self.speaker_sock, self.mic_sock, self.whisper_sock):
            p.parent.mkdir(parents=True, exist_ok=True)
            if p.exists():
                p.unlink()

        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._stop = threading.Event()

        # ---- Speaker state ----
        self._speaker_sessions: list[dict[str, Any]] = []
        self._speaker_seq: int = 0
        self._speaker_active: int = 0
        self._speaker_total_bytes: int = 0
        self._speaker_capture_dir: Path | None = Path(capture_dir) if capture_dir else None
        self._speaker_fault: dict[str, Any] = {
            "pace_realtime": True,
            "stall_after_bytes": None,
            "exit_early_code": None,
        }

        # ---- Echo state (speaker -> mic coupling) ----
        self._echo_cfg: dict[str, Any] = {
            "enabled": False,
            "gain": 0.6,
            "delay_ms": 40,
            "file_path": None,
        }
        # Deque of (deliver_mono_s, array('h') of 16k mono samples)
        self._echo_pending: collections.deque[tuple[float, array.array]] = collections.deque()
        self._echo_samples: collections.deque[int] = collections.deque()
        self._echo_frames_mixed: int = 0

        # ---- Mic state ----
        self._mic_clients: list[dict[str, Any]] = []
        self._mic_spawns: int = 0
        self._mic_exits: int = 0
        self._mic_frames_sent: int = 0
        self._mic_last_argv: list[str] | None = None
        self._mic_noise_rms: float = 0.0
        self._mic_fault: dict[str, Any] = {"stall": False, "exit_code": None}
        # Queue of clips: {"clip_id", "samples": array('h'), "pos": int, "target_channel": int}
        self._mic_queue: collections.deque[dict[str, Any]] = collections.deque()
        self._mic_clips_queued: int = 0
        self._mic_clips_completed: int = 0
        self._mic_completed_ids: list[str] = []

        # ---- Slow Whisper state ----
        self._whisper_cfg: dict[str, Any] = {
            "real_whisper_bin": str(real_whisper_bin),
            "delay_before_ms": 0,
            "delay_after_ms": 0,
            "hold": False,
            "exit_code": None,
        }
        self._whisper_calls: list[dict[str, Any]] = []
        self._whisper_seq: int = 0
        self._whisper_active: int = 0
        self._whisper_hold_events: dict[int, threading.Event] = {}

        # ---- Sockets & threads ----
        self._speaker_srv = self._listen_unix(self.speaker_sock)
        self._mic_srv = self._listen_unix(self.mic_sock)
        self._whisper_srv = self._listen_unix(self.whisper_sock)

        self._threads = [
            threading.Thread(target=self._accept_speaker, name="speaker-accept", daemon=True),
            threading.Thread(target=self._accept_mic, name="mic-accept", daemon=True),
            threading.Thread(target=self._accept_whisper, name="whisper-accept", daemon=True),
            threading.Thread(target=self._mic_ticker_loop, name="mic-ticker", daemon=True),
        ]
        for t in self._threads:
            t.start()

    @staticmethod
    def _listen_unix(path: Path) -> socket.socket:
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(str(path))
        os.chmod(path, 0o660)
        srv.listen(16)
        srv.settimeout(0.2)
        return srv

    def close(self) -> None:
        self._stop.set()
        with self._cond:
            for ev in self._whisper_hold_events.values():
                ev.set()
            self._cond.notify_all()
        for srv in (self._speaker_srv, self._mic_srv, self._whisper_srv):
            try:
                srv.close()
            except OSError:
                pass
        for t in self._threads:
            t.join(timeout=1.5)
        for p in (self.speaker_sock, self.mic_sock, self.whisper_sock):
            try:
                if p.exists():
                    p.unlink()
            except OSError:
                pass

    # =========================================================================
    # Speaker (aplay) server
    # =========================================================================

    def _accept_speaker(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._speaker_srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(
                target=self._handle_speaker_client,
                args=(conn,),
                name="speaker-client",
                daemon=True,
            ).start()

    def _handle_speaker_client(self, conn: socket.socket) -> None:
        session: dict[str, Any] | None = None
        capture_fp = None
        ended_cleanly = False
        try:
            conn.settimeout(10.0)
            start_msg = json.loads(_recv_line(conn).decode("utf-8"))
            rate = int(start_msg.get("rate", 24000))
            channels = int(start_msg.get("channels", 1))
            with self._cond:
                self._speaker_seq += 1
                sid = self._speaker_seq
                self._speaker_active += 1
                cap_path: str | None = None
                if self._speaker_capture_dir is not None:
                    self._speaker_capture_dir.mkdir(parents=True, exist_ok=True)
                    cap_path = str(self._speaker_capture_dir / f"speaker_{sid:04d}.pcm")
                    capture_fp = open(cap_path, "wb")
                session = {
                    "session_id": sid,
                    "pid": start_msg.get("pid"),
                    "args": start_msg.get("args", []),
                    "rate": rate,
                    "channels": channels,
                    "buffer_us": start_msg.get("buffer_us"),
                    "device": start_msg.get("device"),
                    "started_ms": start_msg.get("started_ms", _now_ms()),
                    "first_chunk_ms": None,
                    "eof_ms": None,
                    "end_ms": None,
                    "term_ms": None,
                    "total_bytes": 0,
                    "first_byte": None,
                    "last_byte": None,
                    "chunks_count": 0,
                    "last_rms": 0.0,
                    "killed": False,
                    "exit_code": None,
                    "capture_path": cap_path,
                }
                self._speaker_sessions.append(session)
                if len(self._speaker_sessions) > 500:
                    del self._speaker_sessions[: len(self._speaker_sessions) - 500]
                reply_cfg = {
                    "session_id": sid,
                    "pace_realtime": self._speaker_fault["pace_realtime"],
                    "stall_after_bytes": self._speaker_fault["stall_after_bytes"],
                    "exit_early_code": self._speaker_fault["exit_early_code"],
                    "send_pcm": bool(
                        self._echo_cfg["enabled"] or self._speaker_capture_dir is not None
                    ),
                }
                echo_file = self._echo_cfg["file_path"] if self._echo_cfg["enabled"] else None
                echo_gain = float(self._echo_cfg["gain"])
                echo_delay_s = max(0.0, float(self._echo_cfg["delay_ms"]) / 1000.0)
                self._cond.notify_all()

            conn.sendall((json.dumps(reply_cfg) + "\n").encode("utf-8"))

            if echo_file:
                try:
                    samples = load_pcm_16k_mono(Path(echo_file), gain=echo_gain)
                    with self._lock:
                        self._echo_pending.append((time.monotonic() + echo_delay_s, samples))
                except Exception:
                    pass

            conn.settimeout(None)
            while not self._stop.is_set():
                header_bytes = _recv_line(conn)
                if not header_bytes:
                    break
                msg = json.loads(header_bytes.decode("utf-8"))
                mtype = msg.get("type")
                if mtype == "chunk":
                    n = int(msg.get("n", 0))
                    raw_pcm = b""
                    if msg.get("has_pcm") and n > 0:
                        raw_pcm = _recv_exact(conn, n)
                    if capture_fp is not None and raw_pcm:
                        capture_fp.write(raw_pcm)
                    now = _now_ms()
                    with self._cond:
                        if session["first_chunk_ms"] is None:
                            session["first_chunk_ms"] = now
                        session["total_bytes"] += n
                        session["chunks_count"] += 1
                        if session["first_byte"] is None and msg.get("b") is not None:
                            session["first_byte"] = int(msg["b"])
                        if msg.get("b") is not None:
                            session["last_byte"] = int(msg["b"])
                        if raw_pcm:
                            session["last_rms"] = _pcm_rms(raw_pcm)
                        self._speaker_total_bytes += n
                        if raw_pcm and self._echo_cfg["enabled"] and not self._echo_cfg["file_path"]:
                            self._enqueue_live_echo_locked(raw_pcm, rate, channels)
                        self._cond.notify_all()
                elif mtype == "eof":
                    with self._cond:
                        session["eof_ms"] = msg.get("ts_ms", _now_ms())
                        self._cond.notify_all()
                elif mtype == "term":
                    with self._cond:
                        session["term_ms"] = msg.get("ts_ms", _now_ms())
                        session["killed"] = True
                        session["exit_code"] = msg.get("exit_code", 143)
                        self._cond.notify_all()
                elif mtype == "end":
                    ended_cleanly = True
                    with self._cond:
                        session["end_ms"] = msg.get("ts_ms", _now_ms())
                        session["exit_code"] = msg.get("exit_code", 0)
                        self._cond.notify_all()
                    break
        except (EOFError, OSError, ValueError, json.JSONDecodeError):
            pass
        finally:
            if capture_fp is not None:
                try:
                    capture_fp.close()
                except OSError:
                    pass
            try:
                conn.close()
            except OSError:
                pass
            if session is not None:
                with self._cond:
                    if not ended_cleanly and session["end_ms"] is None:
                        session["end_ms"] = _now_ms()
                        session["killed"] = True
                    if self._speaker_active > 0:
                        self._speaker_active -= 1
                    self._cond.notify_all()

    def _enqueue_live_echo_locked(self, raw_pcm: bytes, rate: int, channels: int) -> None:
        if len(raw_pcm) < 2 * channels:
            return
        usable = raw_pcm[: len(raw_pcm) - (len(raw_pcm) % (2 * channels))]
        arr = array.array("h")
        arr.frombytes(usable)
        if channels > 1:
            mono = array.array("h", arr[0::channels])
        else:
            mono = arr
        resampled = _resample_i16_mono(mono, rate, MIC_RATE)
        gain = float(self._echo_cfg["gain"])
        if abs(gain - 1.0) > 1e-6:
            for i in range(len(resampled)):
                resampled[i] = _clamp_i16(int(round(resampled[i] * gain)))
        delay_s = max(0.0, float(self._echo_cfg["delay_ms"]) / 1000.0)
        self._echo_pending.append((time.monotonic() + delay_s, resampled))

    # =========================================================================
    # Microphone (arecord) server & 20 ms realtime ticker
    # =========================================================================

    def _accept_mic(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._mic_srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            try:
                conn.settimeout(5.0)
                meta = json.loads(_recv_line(conn).decode("utf-8"))
                conn.settimeout(1.0)
                client = {
                    "conn": conn,
                    "pid": meta.get("pid"),
                    "argv": meta.get("argv", []),
                    "channels": max(1, min(8, int(meta.get("channels", 1)))),
                    "rate": int(meta.get("rate", MIC_RATE)),
                    "device": meta.get("device", "default"),
                    "connected_ms": _now_ms(),
                }
                with self._cond:
                    self._mic_clients.append(client)
                    self._mic_spawns += 1
                    self._mic_last_argv = client["argv"]
                    self._cond.notify_all()
            except Exception:
                try:
                    conn.close()
                except OSError:
                    pass

    def _mic_ticker_loop(self) -> None:
        rng = random.Random(42)
        next_tick = time.monotonic()
        while not self._stop.is_set():
            next_tick += MIC_FRAME_PERIOD_S
            now = time.monotonic()
            if next_tick > now:
                time.sleep(next_tick - now)
            else:
                next_tick = now

            with self._cond:
                clients = list(self._mic_clients)
                stalled = bool(self._mic_fault["stall"])
                fault_exit = self._mic_fault["exit_code"]
                noise_rms = float(self._mic_noise_rms)
                echo_enabled = bool(self._echo_cfg["enabled"])

                # Always drain scheduled echo chunks whose delay has elapsed
                now_mono = time.monotonic()
                while self._echo_pending and self._echo_pending[0][0] <= now_mono:
                    _, samples = self._echo_pending.popleft()
                    self._echo_samples.extend(samples)

                if not clients:
                    # Keep echo buffer bounded when no mic is listening
                    if len(self._echo_samples) > MIC_RATE * 2:
                        for _ in range(len(self._echo_samples) - MIC_RATE * 2):
                            self._echo_samples.popleft()
                    continue

                if fault_exit is not None:
                    self._mic_fault["exit_code"] = None
                    dead_clients = clients
                    self._mic_clients.clear()
                    self._mic_exits += len(dead_clients)
                    self._cond.notify_all()
                else:
                    dead_clients = []

            if fault_exit is not None:
                pkt = struct.pack("!BI", 2, 1) + bytes([int(fault_exit) & 0xFF])
                for c in dead_clients:
                    try:
                        c["conn"].sendall(pkt)
                    except OSError:
                        pass
                    try:
                        c["conn"].close()
                    except OSError:
                        pass
                continue

            if stalled:
                continue

            # Build one 320-sample mono frame (clip + echo + noise)
            mono = array.array("h", [0]) * MIC_FRAME_SAMPLES
            target_channel = 0
            completed_clip_id: str | None = None
            mixed_echo = False

            with self._cond:
                if self._mic_queue:
                    clip = self._mic_queue[0]
                    target_channel = int(clip["target_channel"])
                    samples = clip["samples"]
                    pos = clip["pos"]
                    avail = min(MIC_FRAME_SAMPLES, len(samples) - pos)
                    for i in range(avail):
                        mono[i] = samples[pos + i]
                    clip["pos"] = pos + avail
                    if clip["pos"] >= len(samples):
                        finished = self._mic_queue.popleft()
                        self._mic_clips_completed += 1
                        completed_clip_id = finished.get("clip_id")
                        if completed_clip_id:
                            self._mic_completed_ids.append(completed_clip_id)
                        self._cond.notify_all()

                if echo_enabled and self._echo_samples:
                    mixed_echo = True
                    for i in range(MIC_FRAME_SAMPLES):
                        if not self._echo_samples:
                            break
                        mono[i] = _clamp_i16(mono[i] + self._echo_samples.popleft())
                    self._echo_frames_mixed += 1

            if noise_rms > 0.0:
                amp = noise_rms * 1.732
                for i in range(MIC_FRAME_SAMPLES):
                    nval = int(round(rng.uniform(-amp, amp)))
                    mono[i] = _clamp_i16(mono[i] + nval)

            # Dispatch interleaved PCM frame to each connected arecord client
            failed: list[dict[str, Any]] = []
            for c in clients:
                ch = c["channels"]
                if ch == 1:
                    payload = mono.tobytes()
                else:
                    inter = array.array("h", [0]) * (MIC_FRAME_SAMPLES * ch)
                    tc = max(0, min(ch - 1, target_channel))
                    for i in range(MIC_FRAME_SAMPLES):
                        inter[i * ch + tc] = mono[i]
                    payload = inter.tobytes()
                try:
                    c["conn"].sendall(struct.pack("!BI", 1, len(payload)) + payload)
                except OSError:
                    failed.append(c)

            with self._cond:
                self._mic_frames_sent += 1
                if failed:
                    for c in failed:
                        if c in self._mic_clients:
                            self._mic_clients.remove(c)
                            self._mic_exits += 1
                        try:
                            c["conn"].close()
                        except OSError:
                            pass
                    self._cond.notify_all()

    # =========================================================================
    # Slow real Whisper wrapper server
    # =========================================================================

    def _accept_whisper(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._whisper_srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(
                target=self._handle_whisper_client,
                args=(conn,),
                name="whisper-client",
                daemon=True,
            ).start()

    def _handle_whisper_client(self, conn: socket.socket) -> None:
        call: dict[str, Any] | None = None
        hold_ev: threading.Event | None = None
        call_id: int | None = None
        finished = False
        try:
            conn.settimeout(10.0)
            start_msg = json.loads(_recv_line(conn).decode("utf-8"))
            with self._cond:
                self._whisper_seq += 1
                call_id = self._whisper_seq
                self._whisper_active += 1
                cfg = dict(self._whisper_cfg)
                call = {
                    "id": call_id,
                    "pid": start_msg.get("pid"),
                    "argv": start_msg.get("argv", []),
                    "wav_bytes": int(start_msg.get("wav_bytes", 0)),
                    "wav_duration_ms": float(start_msg.get("wav_duration_ms", 0.0)),
                    "started_ms": _now_ms(),
                    "finished_ms": None,
                    "duration_ms": None,
                    "exit_code": None,
                    "stdout_bytes": None,
                    "cancelled": False,
                    "held": bool(cfg["hold"]),
                    "delay_before_ms": int(cfg["delay_before_ms"]),
                    "delay_after_ms": int(cfg["delay_after_ms"]),
                    "real_whisper_bin": cfg["real_whisper_bin"],
                }
                self._whisper_calls.append(call)
                if len(self._whisper_calls) > 200:
                    del self._whisper_calls[: len(self._whisper_calls) - 200]
                if cfg["hold"]:
                    hold_ev = threading.Event()
                    self._whisper_hold_events[call_id] = hold_ev
                self._cond.notify_all()

            reply = {
                "call_id": call_id,
                "real_whisper_bin": cfg["real_whisper_bin"],
                "delay_before_ms": int(cfg["delay_before_ms"]),
                "delay_after_ms": int(cfg["delay_after_ms"]),
                "hold": bool(cfg["hold"]),
                "exit_code": cfg["exit_code"],
            }
            conn.sendall((json.dumps(reply) + "\n").encode("utf-8"))

            if hold_ev is not None:
                # Wait for either whisper.release OR early client death (kill_on_drop)
                conn.settimeout(0.05)
                while not hold_ev.is_set() and not self._stop.is_set():
                    try:
                        peek = conn.recv(1, socket.MSG_PEEK)
                        if peek == b"":
                            return
                    except socket.timeout:
                        pass
                    except OSError:
                        return
                if not self._stop.is_set():
                    conn.sendall(b'{"release":true}\n')

            conn.settimeout(None)
            done_bytes = _recv_line(conn)
            if done_bytes:
                done = json.loads(done_bytes.decode("utf-8"))
                finished = True
                now = _now_ms()
                with self._cond:
                    call["finished_ms"] = now
                    call["duration_ms"] = round(now - call["started_ms"], 2)
                    call["exit_code"] = done.get("exit_code")
                    call["stdout_bytes"] = done.get("stdout_bytes")
                    call["cancelled"] = False
                    self._cond.notify_all()
        except (EOFError, OSError, ValueError, json.JSONDecodeError):
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass
            if call is not None:
                with self._cond:
                    if call_id is not None:
                        self._whisper_hold_events.pop(call_id, None)
                    if not finished and call["finished_ms"] is None:
                        now = _now_ms()
                        call["finished_ms"] = now
                        call["duration_ms"] = round(now - call["started_ms"], 2)
                        call["cancelled"] = True
                    if self._whisper_active > 0:
                        self._whisper_active -= 1
                    self._cond.notify_all()

    # =========================================================================
    # Public control API
    # =========================================================================

    def speaker_status(self) -> dict[str, Any]:
        with self._lock:
            return {
                "active_sessions": self._speaker_active,
                "total_sessions": self._speaker_seq,
                "total_bytes": self._speaker_total_bytes,
                "echo": dict(self._echo_cfg),
                "fault": dict(self._speaker_fault),
                "capture_dir": str(self._speaker_capture_dir) if self._speaker_capture_dir else None,
                "sessions": list(self._speaker_sessions[-100:]),
            }

    def speaker_clear(self) -> dict[str, Any]:
        with self._lock:
            self._speaker_sessions.clear()
            return {"cleared": True, "latest_session_id": self._speaker_seq}

    def speaker_set_fault(
        self,
        pace_realtime: bool | None = None,
        stall_after_bytes: int | None = None,
        exit_early_code: int | None = None,
        clear_faults: bool = False,
    ) -> dict[str, Any]:
        with self._lock:
            if clear_faults:
                self._speaker_fault = {
                    "pace_realtime": True,
                    "stall_after_bytes": None,
                    "exit_early_code": None,
                }
            if pace_realtime is not None:
                self._speaker_fault["pace_realtime"] = bool(pace_realtime)
            if stall_after_bytes is not None:
                self._speaker_fault["stall_after_bytes"] = int(stall_after_bytes)
            if exit_early_code is not None:
                self._speaker_fault["exit_early_code"] = int(exit_early_code)
            return dict(self._speaker_fault)

    def speaker_set_capture_dir(self, directory: str | None) -> dict[str, Any]:
        with self._lock:
            self._speaker_capture_dir = Path(directory) if directory else None
            return {
                "capture_dir": str(self._speaker_capture_dir) if self._speaker_capture_dir else None
            }

    def speaker_wait_for(
        self,
        event: str = "end",
        min_bytes: int = 0,
        after_session: int = 0,
        timeout_ms: int = 10000,
    ) -> dict[str, Any]:
        deadline = time.monotonic() + (timeout_ms / 1000.0)
        with self._cond:
            while True:
                for s in self._speaker_sessions:
                    if s["session_id"] <= after_session:
                        continue
                    if s["total_bytes"] < min_bytes and event != "start":
                        continue
                    if event == "start":
                        return {"matched": True, "session": dict(s)}
                    if event == "chunk" and s["first_chunk_ms"] is not None:
                        return {"matched": True, "session": dict(s)}
                    if event == "eof" and s["eof_ms"] is not None:
                        return {"matched": True, "session": dict(s)}
                    if event == "end" and s["end_ms"] is not None:
                        return {"matched": True, "session": dict(s)}
                    if event == "killed" and s["killed"]:
                        return {"matched": True, "session": dict(s)}
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"speaker.wait_for({event}) timed out after {timeout_ms}ms "
                        f"(after_session={after_session}, sessions={len(self._speaker_sessions)})"
                    )
                self._cond.wait(timeout=min(remaining, 0.05))

    def mic_status(self) -> dict[str, Any]:
        with self._lock:
            queued_samples = sum(len(c["samples"]) - c["pos"] for c in self._mic_queue)
            return {
                "active_clients": len(self._mic_clients),
                "spawns": self._mic_spawns,
                "exits": self._mic_exits,
                "frames_sent": self._mic_frames_sent,
                "queue_len": len(self._mic_queue),
                "queued_ms": round(queued_samples * 1000.0 / MIC_RATE, 1),
                "clips_queued": self._mic_clips_queued,
                "clips_completed": self._mic_clips_completed,
                "completed_ids": list(self._mic_completed_ids[-50:]),
                "noise_rms": self._mic_noise_rms,
                "echo": dict(self._echo_cfg),
                "echo_frames_mixed": self._echo_frames_mixed,
                "fault": dict(self._mic_fault),
                "last_argv": self._mic_last_argv,
            }

    def mic_play_samples(
        self,
        samples: array.array,
        clip_id: str | None = None,
        pad_silence_before_ms: int = 200,
        pad_silence_after_ms: int = 1200,
        target_channel: int = 0,
        replace: bool = False,
    ) -> dict[str, Any]:
        before_n = max(0, int(round(pad_silence_before_ms * MIC_RATE / 1000.0)))
        after_n = max(0, int(round(pad_silence_after_ms * MIC_RATE / 1000.0)))
        padded = array.array("h", [0]) * before_n
        padded.extend(samples)
        padded.extend(array.array("h", [0]) * after_n)

        with self._cond:
            if replace:
                self._mic_queue.clear()
            self._mic_clips_queued += 1
            cid = clip_id or f"clip-{self._mic_clips_queued}"
            self._mic_queue.append(
                {
                    "clip_id": cid,
                    "samples": padded,
                    "pos": 0,
                    "target_channel": int(target_channel),
                }
            )
            self._cond.notify_all()
            return {
                "clip_id": cid,
                "audio_samples": len(samples),
                "total_samples": len(padded),
                "duration_ms": round(len(padded) * 1000.0 / MIC_RATE, 1),
                "queue_len": len(self._mic_queue),
                "active_clients": len(self._mic_clients),
            }

    def mic_play_file(
        self,
        path: str,
        fmt: str | None = None,
        sample_rate: int = MIC_RATE,
        channels: int = 1,
        source_channel: int = 0,
        gain: float = 1.0,
        clip_id: str | None = None,
        pad_silence_before_ms: int = 200,
        pad_silence_after_ms: int = 1200,
        target_channel: int = 0,
        replace: bool = False,
    ) -> dict[str, Any]:
        samples = load_pcm_16k_mono(
            Path(path),
            fmt=fmt,
            sample_rate=sample_rate,
            channels=channels,
            source_channel=source_channel,
            gain=gain,
        )
        return self.mic_play_samples(
            samples,
            clip_id=clip_id,
            pad_silence_before_ms=pad_silence_before_ms,
            pad_silence_after_ms=pad_silence_after_ms,
            target_channel=target_channel,
            replace=replace,
        )

    def mic_play_pcm_b64(
        self,
        pcm_b64: str,
        gain: float = 1.0,
        clip_id: str | None = None,
        pad_silence_before_ms: int = 200,
        pad_silence_after_ms: int = 1200,
        target_channel: int = 0,
        replace: bool = False,
    ) -> dict[str, Any]:
        raw = base64.b64decode(pcm_b64)
        if len(raw) % 2 != 0:
            raw = raw[:-1]
        arr = array.array("h")
        arr.frombytes(raw)
        if abs(gain - 1.0) > 1e-6:
            g = float(gain)
            for i in range(len(arr)):
                arr[i] = _clamp_i16(int(round(arr[i] * g)))
        return self.mic_play_samples(
            arr,
            clip_id=clip_id,
            pad_silence_before_ms=pad_silence_before_ms,
            pad_silence_after_ms=pad_silence_after_ms,
            target_channel=target_channel,
            replace=replace,
        )

    def mic_stop(self) -> dict[str, Any]:
        with self._cond:
            dropped = len(self._mic_queue)
            self._mic_queue.clear()
            self._echo_pending.clear()
            self._echo_samples.clear()
            self._cond.notify_all()
            return {"stopped": True, "dropped_clips": dropped}

    def mic_set_noise(self, rms: float) -> dict[str, Any]:
        with self._lock:
            self._mic_noise_rms = max(0.0, float(rms))
            return {"noise_rms": self._mic_noise_rms}

    def set_echo(
        self,
        enabled: bool | None = None,
        gain: float | None = None,
        delay_ms: int | None = None,
        file_path: str | None = None,
        clear_file_path: bool = False,
        inject_now: bool = False,
    ) -> dict[str, Any]:
        with self._lock:
            if enabled is not None:
                self._echo_cfg["enabled"] = bool(enabled)
            if gain is not None:
                self._echo_cfg["gain"] = float(gain)
            if delay_ms is not None:
                self._echo_cfg["delay_ms"] = max(0, int(delay_ms))
            if clear_file_path:
                self._echo_cfg["file_path"] = None
            elif file_path is not None:
                self._echo_cfg["file_path"] = str(file_path)
            cfg_copy = dict(self._echo_cfg)

        if inject_now and cfg_copy.get("file_path"):
            samples = load_pcm_16k_mono(Path(cfg_copy["file_path"]), gain=float(cfg_copy["gain"]))
            delay_s = max(0.0, float(cfg_copy["delay_ms"]) / 1000.0)
            with self._lock:
                self._echo_cfg["enabled"] = True
                self._echo_pending.append((time.monotonic() + delay_s, samples))
                cfg_copy = dict(self._echo_cfg)
        return cfg_copy

    def mic_set_fault(
        self, stall: bool | None = None, exit_code: int | None = None
    ) -> dict[str, Any]:
        with self._cond:
            if stall is not None:
                self._mic_fault["stall"] = bool(stall)
            if exit_code is not None:
                self._mic_fault["exit_code"] = int(exit_code)
            self._cond.notify_all()
            return dict(self._mic_fault)

    def mic_wait_idle(self, timeout_ms: int = 30000) -> dict[str, Any]:
        deadline = time.monotonic() + (timeout_ms / 1000.0)
        with self._cond:
            while self._mic_queue:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"mic.wait_idle timed out after {timeout_ms}ms")
                self._cond.wait(timeout=min(remaining, 0.1))
            return {
                "idle": True,
                "clips_completed": self._mic_clips_completed,
                "frames_sent": self._mic_frames_sent,
            }

    # =========================================================================
    # Slow real Whisper control API
    # =========================================================================

    def whisper_status(self) -> dict[str, Any]:
        with self._lock:
            return {
                "real_whisper_bin": self._whisper_cfg["real_whisper_bin"],
                "config": dict(self._whisper_cfg),
                "active_calls": self._whisper_active,
                "active_count": self._whisper_active,
                "total_calls": self._whisper_seq,
                "calls": list(self._whisper_calls[-100:]),
            }

    def whisper_configure(
        self,
        real_whisper_bin: str | None = None,
        delay_ms: int | None = None,
        delay_before_ms: int | None = None,
        delay_after_ms: int | None = None,
        hold: bool | None = None,
        exit_code: int | None = None,
        clear_exit_code: bool = False,
    ) -> dict[str, Any]:
        with self._cond:
            if real_whisper_bin is not None:
                self._whisper_cfg["real_whisper_bin"] = str(real_whisper_bin)
            if delay_ms is not None and delay_before_ms is None:
                self._whisper_cfg["delay_before_ms"] = max(0, int(delay_ms))
            if delay_before_ms is not None:
                self._whisper_cfg["delay_before_ms"] = max(0, int(delay_before_ms))
            if delay_after_ms is not None:
                self._whisper_cfg["delay_after_ms"] = max(0, int(delay_after_ms))
            if hold is not None:
                self._whisper_cfg["hold"] = bool(hold)
                if not hold:
                    for ev in self._whisper_hold_events.values():
                        ev.set()
            if clear_exit_code:
                self._whisper_cfg["exit_code"] = None
            elif exit_code is not None:
                self._whisper_cfg["exit_code"] = int(exit_code)
            self._cond.notify_all()
            return dict(self._whisper_cfg)

    def whisper_release(self, call_id: int | None = None) -> dict[str, Any]:
        released: list[int] = []
        with self._cond:
            if call_id is not None:
                ev = self._whisper_hold_events.get(int(call_id))
                if ev is not None:
                    ev.set()
                    released.append(int(call_id))
            else:
                for cid, ev in list(self._whisper_hold_events.items()):
                    ev.set()
                    released.append(cid)
            self._cond.notify_all()
        return {"released": released}

    def whisper_wait_for(
        self,
        event: str = "started",
        after_id: int = 0,
        timeout_ms: int = 30000,
    ) -> dict[str, Any]:
        deadline = time.monotonic() + (timeout_ms / 1000.0)
        with self._cond:
            while True:
                for c in self._whisper_calls:
                    if c["id"] <= after_id:
                        continue
                    if event == "started":
                        return {"matched": True, "call": dict(c)}
                    if event == "finished" and c["finished_ms"] is not None and not c["cancelled"]:
                        return {"matched": True, "call": dict(c)}
                    if event == "cancelled" and c["cancelled"]:
                        return {"matched": True, "call": dict(c)}
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(
                        f"whisper.wait_for({event}) timed out after {timeout_ms}ms "
                        f"(after_id={after_id}, calls={len(self._whisper_calls)})"
                    )
                self._cond.wait(timeout=min(remaining, 0.05))

    def whisper_clear(self) -> dict[str, Any]:
        with self._lock:
            self._whisper_calls.clear()
            return {"cleared": True, "latest_id": self._whisper_seq}
