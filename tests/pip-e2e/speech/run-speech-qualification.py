#!/usr/bin/env python3
"""
ARM64 Pip Speech Qualification Suite.

Exercises REAL Linux ARM64 binaries and models:
  - whisper-cli (ggml-org/whisper.cpp v1.9.6) + ggml-tiny.en.bin
  - ONNX Runtime 1.20.1 (libonnxruntime.so, ORT_API_VERSION=20)
  - openWakeWord v0.5.1 (melspectrogram.onnx + embedding_model.onnx)
  - Private hey_pip.onnx (read-only mounted from local clone; never copied/committed)

Tests:
  1. Architecture, ELF, shared-library, and SHA-256 verification
  2. Real whisper-cli stdin RIFF WAV transcription on human-recorded + synthetic clips
  3. Sample-accurate VAD (including 700-frame sustained >500 RMS ambient recovery & 12s bound)
     feeding real whisper-cli + Pip Policy decision assertions
  4. Real ONNX Runtime neural wake scoring (warmup suppression, human negative rejection,
     ambient noise rejection, opt-in human positive wake / reference parity, synthetic diagnostic)
  5. Echo suppression & post-playback ECHO_TAIL=350ms + real human follow-up qualification
  6. Slow real whisper-cli wrapper tests (onset-scoped follow-up, priority wake cancellation,
     full clip queue replacement, zero orphan whisper-cli processes)
  7. Live IPC check against tests/pip-e2e/peripherals/daemon.py when available

Reliance on fixtures vs real hardware and any missing human recordings are reported
explicitly and separately in the output JSON report.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import platform
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave
from collections import deque
from dataclasses import dataclass
from pathlib import Path
from typing import Any

RATE = 16_000
FRAME = 320  # 20 ms
MAX_FRAMES = 600  # 12 s
END_FRAMES = 40  # 800 ms
FOLLOW_S = 4.0
ECHO_TAIL_S = 0.350
MAX_LINE = 4096

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parents[2]
PR_SET_PDEATHSIG = 1


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def build_riff_wav(samples: list[int]) -> bytes:
    """Exact RIFF WAV builder matching apps/robot/src/listen/io.rs::wav."""
    raw = struct.pack(f"<{len(samples)}h", *samples)
    nbytes = len(raw)
    header = struct.pack(
        "<4sI4s4sIHHIIHH4sI",
        b"RIFF",
        nbytes + 36,
        b"WAVE",
        b"fmt ",
        16,
        1,  # PCM
        1,  # mono
        RATE,
        RATE * 2,
        2,
        16,
        b"data",
        nbytes,
    )
    return header + raw


def read_wav_samples(path: Path) -> list[int]:
    with wave.open(str(path), "rb") as w:
        if w.getnchannels() != 1 or w.getsampwidth() != 2 or w.getframerate() != RATE:
            raise ValueError(f"{path} must be 16kHz mono S16_LE")
        raw = w.readframes(w.getnframes())
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


# ---------------------------------------------------------------------------
# Sample-accurate mirror of apps/robot/src/listen/vad.rs
# ---------------------------------------------------------------------------
@dataclass
class VadClip:
    started_s: float
    pcm: list[int]


class Vad:
    def __init__(self) -> None:
        self.lead: deque[list[int]] = deque(maxlen=8)
        self.recording: VadClip | None = None
        self.voiced: int = 0
        self.quiet: int = 0
        self.noise_power: int = 0
        self.saturated: bool = False

    def reset(self) -> None:
        self.lead.clear()
        self.recording = None
        self.voiced = 0
        self.quiet = 0
        self.saturated = False

    def feed(self, frame: list[int], now_s: float, busy: bool = False) -> VadClip | None:
        assert len(frame) == FRAME
        power = sum(abs(s) * abs(s) for s in frame) // FRAME
        threshold = max(self.noise_power * 4, 500 * 500)
        speech = power >= threshold * (2 if busy else 1)
        self.quiet = 0 if speech else self.quiet + 1
        if self.saturated:
            self.noise_power = (self.noise_power * 31 + power) // 32
            self.saturated = self.quiet < END_FRAMES
            return None
        if self.recording is None:
            if not speech:
                self.noise_power = (self.noise_power * 31 + power) // 32
                self.lead.append(list(frame))
                return None
            lead_pcm: list[int] = []
            for f in self.lead:
                lead_pcm.extend(f)
            self.lead.clear()
            self.recording = VadClip(started_s=now_s, pcm=lead_pcm)
            self.voiced = 0
        self.recording.pcm.extend(frame)
        if speech:
            self.voiced += 1
        self.saturated = len(self.recording.pcm) >= MAX_FRAMES * FRAME and self.quiet < END_FRAMES
        if not self.saturated and self.quiet < END_FRAMES:
            return None
        clip = self.recording if self.voiced >= 8 else None
        self.recording = None
        if not self.saturated:
            self.reset()
        return clip


# ---------------------------------------------------------------------------
# Mirror of apps/robot/src/listen/policy.rs
# ---------------------------------------------------------------------------
SEPS = set(" ,.!?:;-–—…\"'“”‘’\t\r\n")
BARE_PIP = {
    "pip", "pipp", "pips", "pippa", "pippy", "pippin", "pipi", "peep", "peeps",
    "pep", "pepp", "peps", "bip", "bipp", "bips", "bib", "bep", "pib", "piff",
    "kip", "fip", "yip",
}
GREETED_PIP = BARE_PIP | {
    "beep", "pit", "pitt", "pic", "pig", "pete", "pup", "pop", "tip", "dip",
    "hip", "sip", "rip", "lip", "nip", "zip", "vip",
}
SILENCE_ARTIFACTS = {
    "blank_audio", "blank audio", "no_speech", "no speech", "you", "thank you",
    "thanks", "thank you very much", "thanks for watching", "thank you for watching",
    "thanks for watching everyone", "thank you for watching everyone",
    "thanks for watching this video", "thank you for watching this video",
    "thanks for listening", "thank you for listening", "please subscribe",
    "like and subscribe", "please like and subscribe", "don't forget to subscribe",
    "subscribe to my channel", "see you next time", "see you in the next video",
    "bye", "bye-bye", "bye bye", "the end",
}
DIGIT_WORDS = {
    "zero": "0", "one": "1", "two": "2", "three": "3", "four": "4",
    "five": "5", "six": "6", "seven": "7", "eight": "8", "nine": "9",
}


def trim_lead(text: str) -> str:
    i = 0
    while i < len(text) and text[i] in SEPS:
        i += 1
    return text[i:]


def next_token(text: str) -> tuple[str, str]:
    text = trim_lead(text)
    end = len(text)
    for i, ch in enumerate(text):
        if ch.isalnum():
            continue
        if ch in ("'", "’") and i + 1 < len(text) and text[i + 1].isalnum():
            continue
        end = i
        break
    return text[:end], trim_lead(text[end:])


def addressed(text: str, busy: bool) -> str | None:
    first, rest = next_token(text)
    if first.lower() in ("hey", "hi"):
        name, body = next_token(rest)
        return body if name.lower() == "pip" else None
    if not busy and first.lower() == "pip":
        return rest
    return None


def strip_possessive(word: str) -> str:
    for suf in ("'s", "’s", "'S", "’S"):
        if word.endswith(suf):
            return word[: -len(suf)]
    return word


def acoustic_addressed(text: str) -> str | None:
    first, rest = next_token(text)
    if first.lower() in ("hey", "hi", "hay"):
        name, body = next_token(rest)
        return body if strip_possessive(name).lower() in GREETED_PIP else None
    return rest if strip_possessive(first).lower() in BARE_PIP else None


def parse_code(text: str) -> str | None:
    digits: list[str] = []
    cleaned = text.rstrip(".!?")
    for part in re.split(r"[\s,\-]+", cleaned):
        if not part:
            continue
        if part.isdigit():
            digits.append(part)
        elif part.lower() in DIGIT_WORDS:
            digits.append(DIGIT_WORDS[part.lower()])
        else:
            return None
        if sum(len(d) for d in digits) > 6:
            return None
    joined = "".join(digits)
    return joined if len(joined) == 6 else None


def is_non_speech_artifact(words: str) -> bool:
    if words.startswith(("[", "(", "*")):
        return True
    if any(c in "♪♫♬♩" for c in words):
        return True
    if words and not any(c.isalnum() for c in words):
        return True
    return False


def is_unaddressed_silence_artifact(text: str) -> bool:
    trimmed = text.strip()
    while trimmed and not (trimmed[0].isalnum() or trimmed[0] in "_'"):
        trimmed = trimmed[1:]
    while trimmed and not (trimmed[-1].isalnum() or trimmed[-1] in "_'"):
        trimmed = trimmed[:-1]
    norm = " ".join(trimmed.split()).lower()
    if norm in SILENCE_ARTIFACTS:
        return True
    return (
        norm.startswith("subtitles by ")
        or norm.startswith("captions by ")
        or norm.startswith("transcribed by ")
        or "amara.org" in norm
    )


@dataclass(frozen=True)
class ListenState:
    busy: bool = False
    confirming: bool = False
    muted: bool = False


class Policy:
    def __init__(self) -> None:
        self.window: tuple[float, float] | None = None
        self.face_deadline: float | None = None
        self.stopped: bool = False
        self.acoustic_window: bool = False

    def acoustic_wake(self, at: float) -> None:
        self.stopped = False
        self.window = (max(0.0, at - 2.0), at + FOLLOW_S)
        self.face_deadline = at + FOLLOW_S
        self.acoustic_window = True

    def acoustic_clip(self, onset: float) -> bool:
        return self.window is not None and self.window[0] <= onset <= self.window[1]

    def changed(self, old: ListenState, state: ListenState, now: float) -> None:
        if (
            old.busy
            and not state.busy
            and not old.muted
            and not state.muted
            and not old.confirming
            and not state.confirming
            and self.face_deadline is not None
            and self.face_deadline >= now
        ):
            return
        self.window = None
        self.acoustic_window = False
        if state.busy or state.muted or state.confirming or old.confirming or old.muted:
            self.face_deadline = None
            self.stopped = False
        elif old.busy and not self.stopped:
            self.window = (now + ECHO_TAIL_S, now + ECHO_TAIL_S + FOLLOW_S)

    def heard(self, text: str, state: ListenState, started: float, now: float) -> list[dict[str, Any]]:
        if state.muted or not text or len(text) > MAX_LINE or any(ord(c) < 32 for c in text):
            return []
        in_window = (
            not state.busy
            and self.window is not None
            and self.window[0] <= started <= self.window[1]
        )
        body = addressed(text, state.busy)
        if body is None and in_window and self.acoustic_window:
            body = acoustic_addressed(text)
        words = body if body is not None else trim_lead(text)
        first, tail = next_token(words)
        confirm = first.lower() == "confirm"
        if state.confirming:
            c = parse_code(tail if confirm else words)
            return [{"control": "Confirm", "value": c}] if c else []
        if confirm and parse_code(tail) is not None:
            return []
        if body is None and not in_window:
            return []
        if is_non_speech_artifact(words):
            return []
        if body is None and is_unaddressed_silence_artifact(words):
            return []
        intent = words.rstrip(".!?").lower()
        if intent == "go to sleep":
            self.stopped = True
            self.window = None
            self.face_deadline = None
            self.acoustic_window = False
            return [{"control": "Sleep"}]
        stop = intent in ("stop", "stop talking", "never mind", "nevermind")
        commands: list[dict[str, Any]] = []
        if body is not None or stop:
            commands.append({"control": "Wake"})
        self.stopped = stop
        self.window = None
        self.face_deadline = None
        self.acoustic_window = False
        if stop:
            commands.append({"control": "Listening", "value": False})
        elif not words:
            self.window = (started, now + FOLLOW_S)
            self.face_deadline = now + FOLLOW_S
        else:
            commands.append({"control": "Say", "value": words})
        return commands


# ---------------------------------------------------------------------------
# Real ARM64 whisper-cli invocation (matches apps/robot/src/listen/io.rs)
# ---------------------------------------------------------------------------
def run_whisper_cli(
    whisper_bin: Path,
    model_path: Path,
    samples: list[int],
    timeout_s: float = 30.0,
    extra_env: dict[str, str] | None = None,
    include_of_devnull: bool = True,
) -> tuple[str, float, int]:
    wav_bytes = build_riff_wav(samples)
    env = {
        "LC_ALL": "C",
        "PATH": os.environ.get("PATH", "/usr/local/bin:/usr/bin:/bin"),
        "OMP_NUM_THREADS": os.environ.get("OMP_NUM_THREADS", "2"),
        "OPENBLAS_NUM_THREADS": os.environ.get("OPENBLAS_NUM_THREADS", "1"),
    }
    if extra_env:
        env.update(extra_env)
    cmd = [
        str(whisper_bin),
        "-m",
        str(model_path),
        "-l",
        "en",
        "-t",
        "2",
        "-np",
        "-nt",
    ]
    if include_of_devnull:
        cmd.extend(["-of", "/dev/null"])
    cmd.extend(["-f", "-"])
    t0 = time.monotonic()
    proc = subprocess.run(
        cmd,
        input=wav_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        timeout=timeout_s,
    )
    elapsed_ms = (time.monotonic() - t0) * 1000.0
    text = " ".join(proc.stdout.decode("utf-8", errors="replace").split())
    return text, round(elapsed_ms, 2), proc.returncode


def normalize_text(s: str) -> str:
    return re.sub(r"[^a-z0-9\s]", " ", s.lower()).strip()


def run_ort_probe(
    probe_bin: Path,
    ort_dylib: Path,
    wake_dir: Path,
    pcm_path: Path,
    busy: bool = False,
    ref_scores: Path | None = None,
) -> dict[str, Any]:
    env = dict(os.environ)
    env["ORT_DYLIB_PATH"] = str(ort_dylib)
    cmd = [str(probe_bin), "--wake-dir", str(wake_dir), "--pcm", str(pcm_path)]
    if busy:
        cmd.append("--busy")
    if ref_scores is not None:
        cmd.extend(["--reference-scores", str(ref_scores)])
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, timeout=60)
    if proc.returncode != 0:
        raise RuntimeError(
            f"ort-wake-probe failed (rc={proc.returncode}): {proc.stderr.decode('utf-8', errors='replace')}"
        )
    return json.loads(proc.stdout.decode("utf-8"))


def count_live_whisper_procs(whisper_bin: Path) -> int:
    proc = subprocess.run(
        ["pgrep", "-f", str(whisper_bin)],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    if proc.returncode != 0:
        return 0
    return len([line for line in proc.stdout.decode().splitlines() if line.strip()])


def make_slow_whisper_wrapper(
    wrapper_path: Path,
    real_whisper: Path,
    delay_ms: int = 0,
    hold_flag: Path | None = None,
    started_flag: Path | None = None,
) -> None:
    """
    Standalone slow-real-whisper wrapper with PR_SET_PDEATHSIG=SIGKILL
    so cancelling the parent leaves zero orphan whisper-cli children.
    """
    script = f"""#!/usr/bin/env python3
import ctypes, os, signal, subprocess, sys, time
from pathlib import Path

PR_SET_PDEATHSIG = 1
try:
    ctypes.CDLL("libc.so.6").prctl(PR_SET_PDEATHSIG, signal.SIGKILL)
except Exception:
    pass

started = {repr(str(started_flag)) if started_flag else 'None'}
hold = {repr(str(hold_flag)) if hold_flag else 'None'}
delay_ms = {delay_ms}
real_bin = {repr(str(real_whisper))}

if started:
    Path(started).write_text(str(os.getpid()), encoding="utf-8")

if delay_ms > 0:
    time.sleep(delay_ms / 1000.0)

while hold and Path(hold).exists():
    time.sleep(0.02)

os.execv(real_bin, [real_bin] + sys.argv[1:])
"""
    wrapper_path.write_text(script, encoding="utf-8")
    wrapper_path.chmod(0o755)


def jsonrpc_call(sock_path: Path, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
    req = {"id": 1, "method": method, "params": params or {}}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(15.0)
        s.connect(str(sock_path))
        s.sendall((json.dumps(req) + "\n").encode("utf-8"))
        buf = b""
        while b"\n" not in buf:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    resp = json.loads(buf.decode("utf-8").splitlines()[0])
    if not resp.get("ok"):
        raise RuntimeError(f"IPC {method} failed: {resp}")
    return resp.get("result", {})


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--speech-dir", type=Path, default=Path(os.environ.get("PIP_E2E_SPEECH_DIR", "/opt/pip-e2e/speech")))
    parser.add_argument("--cache-dir", type=Path, default=Path(os.environ.get("PIP_E2E_CACHE_DIR", "/opt/pip-e2e/cache")))
    parser.add_argument("--artifacts-dir", type=Path, default=Path(os.environ.get("PIP_E2E_ARTIFACTS_DIR", "/opt/pip-e2e/artifacts")))
    parser.add_argument("--private-hey-pip", type=Path, default=Path(os.environ.get("PIP_E2E_HEY_PIP_ONNX", "/opt/pip-e2e/private/hey_pip.onnx")))
    parser.add_argument("--report", type=Path, default=None)
    args = parser.parse_args()

    speech_dir: Path = args.speech_dir
    cache_dir: Path = args.cache_dir
    artifacts_dir: Path = args.artifacts_dir
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    report_path = args.report or (artifacts_dir / "speech-qualification.json")

    whisper_bin = speech_dir / "bin" / "whisper-cli"
    ort_probe_bin = speech_dir / "bin" / "ort-wake-probe"
    ort_dylib = speech_dir / "lib" / "libonnxruntime.so"
    whisper_model = Path(os.environ.get("BUDDY_LISTEN_MODEL", str(cache_dir / "speech" / "models" / "ggml-tiny.en.bin")))
    mel_model = cache_dir / "speech" / "models" / "wake" / "melspectrogram.onnx"
    emb_model = cache_dir / "speech" / "models" / "wake" / "embedding_model.onnx"
    manifest_path = cache_dir / "speech" / "fixtures" / "manifest.json"

    # Ensure models and fixtures are present and verified
    if not manifest_path.is_file() or not whisper_model.is_file() or not mel_model.is_file() or not emb_model.is_file():
        subprocess.run(
            [str(SCRIPT_DIR / "install-speech-stack.sh"), "--prefix", str(speech_dir), "--cache", str(cache_dir), "--fetch-only"],
            check=True,
        )
    else:
        # Refresh manifest so any opt-in env vars (PIP_E2E_HUMAN_*) are picked up immediately
        subprocess.run(
            ["python3", str(SCRIPT_DIR / "prepare-fixtures.py"), "--cache", str(cache_dir), "--lockfile", str(SCRIPT_DIR / "assets.lock.json")],
            check=True,
        )

    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    clips_by_id = {c["id"]: c for c in manifest["clips"]}
    lock = json.loads((SCRIPT_DIR / "assets.lock.json").read_text(encoding="utf-8"))

    results: list[dict[str, Any]] = []

    def record(name: str, status: str, category: str, provenance: str, detail: dict[str, Any]) -> None:
        entry = {
            "name": name,
            "status": status,  # "pass" | "fail" | "missing_evidence" | "diagnostic_only"
            "category": category,
            "provenance": provenance,
            "detail": detail,
        }
        results.append(entry)
        tag = status.upper()
        print(f"[{tag}] {name} ({provenance})")

    # ------------------------------------------------------------------
    # 1. Environment, ARM64 ELF, and SHA-256 verification
    # ------------------------------------------------------------------
    arch = platform.machine()
    artifacts_hashes: dict[str, Any] = {}
    for label, p, expected_sha in [
        ("whisper_cli", whisper_bin, None),
        ("ort_wake_probe", ort_probe_bin, None),
        ("libonnxruntime_so", ort_dylib, lock["runtime"]["onnxruntime_nupkg"]["extracted"]["libonnxruntime.so"]["sha256"]),
        ("ggml_tiny_en", whisper_model, lock["models"]["whisper_tiny_en"]["sha256"] if whisper_model.name == "ggml-tiny.en.bin" else None),
        ("melspectrogram_onnx", mel_model, lock["models"]["openwakeword_melspectrogram"]["sha256"]),
        ("embedding_model_onnx", emb_model, lock["models"]["openwakeword_embedding"]["sha256"]),
    ]:
        exists = p.is_file()
        got = sha256_file(p) if exists else None
        ok = exists and (expected_sha is None or got == expected_sha)
        artifacts_hashes[label] = {
            "path": str(p),
            "exists": exists,
            "size_bytes": p.stat().st_size if exists else 0,
            "sha256": got,
            "expected_sha256": expected_sha,
            "verified": ok,
        }

    hey_pip_path: Path = args.private_hey_pip
    hey_pip_present = hey_pip_path.is_file()
    hey_pip_readonly = False
    if hey_pip_present:
        # Confirm read-only mount or non-writable permission when mounted :ro
        try:
            with hey_pip_path.open("r+b"):
                hey_pip_readonly = False
        except OSError:
            hey_pip_readonly = True
        artifacts_hashes["private_hey_pip_onnx"] = {
            "path": str(hey_pip_path),
            "exists": True,
            "size_bytes": hey_pip_path.stat().st_size,
            "sha256": sha256_file(hey_pip_path),
            "read_only_mount_enforced": hey_pip_readonly,
            "private_asset_never_committed": True,
        }
    else:
        artifacts_hashes["private_hey_pip_onnx"] = {
            "path": str(hey_pip_path),
            "exists": False,
            "opt_in_env": "PIP_E2E_HEY_PIP_ONNX",
        }

    all_public_ok = arch in ("aarch64", "arm64") and all(
        artifacts_hashes[k]["verified"]
        for k in ("whisper_cli", "ort_wake_probe", "libonnxruntime_so", "ggml_tiny_en", "melspectrogram_onnx", "embedding_model_onnx")
    )
    record(
        "arm64_assets_and_checksums",
        "pass" if all_public_ok else "fail",
        "runtime_assets",
        "real_arm64_binary",
        {"arch": arch, "assets": artifacts_hashes},
    )

    # Verify whisper-cli reports non-zero exit cleanly on invalid model path (listen/io.rs error path)
    _, err_ms, bad_rc = run_whisper_cli(whisper_bin, Path("/nonexistent/model.bin"), [0] * RATE)
    record(
        "whisper_cli_invalid_model_fails_cleanly",
        "pass" if bad_rc != 0 else "fail",
        "whisper",
        "real_arm64_binary",
        {"exit_code": bad_rc, "elapsed_ms": err_ms},
    )

    # Verify apps/robot/src/listen/io.rs passes `-of /dev/null` (prevents whisper.cpp >= 1.8 silent stdout on `-f -`)
    io_rs_path = REPO_ROOT / "apps" / "robot" / "src" / "listen" / "io.rs"
    io_rs_text = io_rs_path.read_text(encoding="utf-8") if io_rs_path.is_file() else ""
    io_rs_has_of_devnull = ('"-of"' in io_rs_text and '"/dev/null"' in io_rs_text) or '"-otxt"' in io_rs_text
    jfk_samples_probe = read_wav_samples(Path(clips_by_id["human_jfk_negative"]["path"]))
    bare_text, bare_ms, bare_rc = run_whisper_cli(
        whisper_bin, whisper_model, jfk_samples_probe, include_of_devnull=False
    )
    fixed_text, fixed_ms, fixed_rc = run_whisper_cli(
        whisper_bin, whisper_model, jfk_samples_probe, include_of_devnull=True
    )
    record(
        "whisper_cli_piped_stdout_contract",
        "pass" if (io_rs_has_of_devnull and fixed_rc == 0 and "fellow americans" in normalize_text(fixed_text)) else "fail",
        "whisper",
        "human_recorded",
        {
            "io_rs_guards_stdout_flag": io_rs_has_of_devnull,
            "bare_minus_f_dash_stdout": bare_text,
            "bare_minus_f_dash_rc": bare_rc,
            "bare_minus_f_dash_ms": bare_ms,
            "with_of_devnull_stdout": fixed_text,
            "with_of_devnull_rc": fixed_rc,
            "with_of_devnull_ms": fixed_ms,
        },
    )

    # ------------------------------------------------------------------
    # 2. Real ARM64 whisper-cli transcription on human-recorded clips
    # ------------------------------------------------------------------
    for cid in ("human_jfk_negative", "human_voices_followup", "human_librispeech_negative"):
        clip = clips_by_id[cid]
        samples = read_wav_samples(Path(clip["path"]))
        transcript, elapsed_ms, rc = run_whisper_cli(whisper_bin, whisper_model, samples)
        norm = normalize_text(transcript)
        missing_subs = [sub for sub in clip["expected_substrings"] if sub not in norm]
        # Verify Pip Policy rejects unaddressed human negative speech when idle
        policy = Policy()
        idle_cmds = policy.heard(transcript, ListenState(), 10.0, 10.5)
        # Verify Pip Policy accepts it as Control::Say when inside an active follow-up window
        follow_policy = Policy()
        follow_policy.changed(ListenState(busy=True), ListenState(busy=False), 10.0)
        follow_cmds = follow_policy.heard(transcript, ListenState(), 10.5, 11.0)
        ok = (
            rc == 0
            and not missing_subs
            and idle_cmds == []
            and len(follow_cmds) == 1
            and follow_cmds[0]["control"] == "Say"
        )
        record(
            f"whisper_human_{cid}",
            "pass" if ok else "fail",
            "whisper_human",
            "human_recorded",
            {
                "clip_id": cid,
                "wav_sha256": clip["sha256"],
                "audio_duration_s": clip["audio"]["duration_s"],
                "transcript": transcript,
                "expected_substrings": clip["expected_substrings"],
                "missing_substrings": missing_subs,
                "decode_ms": elapsed_ms,
                "rtf": round((elapsed_ms / 1000.0) / clip["audio"]["duration_s"], 4),
                "idle_policy_commands": idle_cmds,
                "followup_policy_commands": follow_cmds,
            },
        )

    # Optional user-supplied human recordings
    for cid in ("human_wake_positive", "human_opt_in_wake_negative", "human_opt_in_followup"):
        clip = clips_by_id.get(cid)
        if not clip or clip["provenance"] == "missing":
            if cid == "human_wake_positive":
                record(
                    "whisper_human_wake_positive",
                    "missing_evidence",
                    "whisper_human",
                    "missing",
                    {
                        "reason": "No prerecorded human 'Hey Pip' WAV found locally; TTS is NOT substituted as human evidence.",
                        "opt_in_env": "PIP_E2E_HUMAN_WAKE_POSITIVE_WAV",
                        "opt_in_dir": "${PIP_E2E_HUMAN_SPEECH_DIR}/wake_positive.wav",
                    },
                )
            continue
        samples = read_wav_samples(Path(clip["path"]))
        transcript, elapsed_ms, rc = run_whisper_cli(whisper_bin, whisper_model, samples)
        policy = Policy()
        cmds = policy.heard(transcript, ListenState(), 10.0, 10.5)
        ok = rc == 0 and bool(transcript)
        if cid == "human_wake_positive":
            ok = ok and any(c["control"] == "Wake" for c in cmds)
        record(
            f"whisper_{cid}",
            "pass" if ok else "fail",
            "whisper_human",
            "human_recorded",
            {
                "clip_id": cid,
                "wav_sha256": clip["sha256"],
                "transcript": transcript,
                "decode_ms": elapsed_ms,
                "policy_commands": cmds,
            },
        )

    # ------------------------------------------------------------------
    # 3. Synthetic TTS + Policy decision assertions (explicitly non-human!)
    # ------------------------------------------------------------------
    tts_expectations = [
        ("synth_tts_hey_pip_sleep", ListenState(), "idle", "sleep"),
        ("synth_tts_third_party_pip", ListenState(), "idle", "reject"),
        ("synth_tts_followup_tomorrow", ListenState(), "followup", "say_tomorrow"),
        ("synth_tts_confirm_code", ListenState(confirming=True), "idle", "confirm_012345"),
        ("synth_tts_acoustic_misheard_question", ListenState(), "acoustic_wake", "acoustic_say_time"),
        ("synth_tts_acoustic_misheard_stop", ListenState(), "acoustic_wake", "acoustic_stop"),
    ]
    for cid, st, mode_window, expect_kind in tts_expectations:
        clip = clips_by_id.get(cid)
        if not clip:
            record(
                f"vad_whisper_policy_{cid}",
                "missing_evidence",
                "vad_whisper_policy",
                "missing",
                {"reason": "espeak-ng unavailable to synthesize pipeline clip"},
            )
            continue
        samples = read_wav_samples(Path(clip["path"]))
        vad = Vad()
        emitted: list[VadClip] = []
        for idx in range(0, len(samples) - FRAME + 1, FRAME):
            c = vad.feed(samples[idx : idx + FRAME], idx / RATE, st.busy)
            if c is not None:
                emitted.append(c)
        if not emitted:
            record(
                f"vad_whisper_policy_{cid}",
                "fail",
                "vad_whisper_policy",
                "synthetic",
                {"reason": "VAD did not emit a clip for synthetic utterance"},
            )
            continue
        transcript, elapsed_ms, rc = run_whisper_cli(whisper_bin, whisper_model, emitted[0].pcm)
        policy = Policy()
        if mode_window == "followup":
            policy.changed(ListenState(busy=True), ListenState(busy=False), 0.0)
            cmds = policy.heard(transcript, st, 0.5, 1.0)
        elif mode_window == "acoustic_wake":
            policy.acoustic_wake(1.0)
            cmds = policy.heard(transcript, st, 1.1, 1.6)
            # In acoustic mode (mod.rs lines 754-756), Control::Wake was already emitted by the
            # acoustic detector and duplicate Control::Wake is stripped from policy commands.
            cmds = [c for c in cmds if c["control"] != "Wake"]
        else:
            cmds = policy.heard(transcript, st, emitted[0].started_s, emitted[0].started_s + 0.5)

        if expect_kind == "sleep":
            ok = rc == 0 and cmds == [{"control": "Sleep"}]
        elif expect_kind == "reject":
            ok = rc == 0 and cmds == []
        elif expect_kind == "say_tomorrow":
            ok = rc == 0 and len(cmds) == 1 and cmds[0]["control"] == "Say" and "tomorrow" in normalize_text(cmds[0]["value"])
        elif expect_kind == "confirm_012345":
            ok = rc == 0 and cmds == [{"control": "Confirm", "value": "012345"}]
        elif expect_kind == "acoustic_say_time":
            ok = rc == 0 and len(cmds) == 1 and cmds[0]["control"] == "Say" and "time" in normalize_text(cmds[0]["value"]) and "pib" not in normalize_text(cmds[0]["value"])
        elif expect_kind == "acoustic_stop":
            ok = rc == 0 and cmds == [{"control": "Listening", "value": False}]
        else:
            ok = False

        record(
            f"vad_whisper_policy_{cid}",
            "pass" if ok else "fail",
            "vad_whisper_policy",
            "synthetic",
            {
                "note": "Synthetic espeak-ng fixture for VAD+Whisper+Policy pipeline mechanics only (NOT human speech).",
                "mode_window": mode_window,
                "expected_decision": expect_kind,
                "vad_clips_emitted": len(emitted),
                "vad_clip_duration_s": round(len(emitted[0].pcm) / RATE, 3),
                "transcript": transcript,
                "decode_ms": elapsed_ms,
                "policy_commands": cmds,
            },
        )

    # Silence & noise -> VAD emits 0 clips; direct whisper-cli on silence yields no actionable command
    for cid in ("synth_silence", "synth_ambient_450_click"):
        clip = clips_by_id[cid]
        samples = read_wav_samples(Path(clip["path"]))
        vad = Vad()
        emitted = [
            c
            for idx in range(0, len(samples) - FRAME + 1, FRAME)
            if (c := vad.feed(samples[idx : idx + FRAME], idx / RATE, False)) is not None
        ]
        transcript, elapsed_ms, rc = run_whisper_cli(whisper_bin, whisper_model, samples)
        # Even inside a follow-up window, silence artifact must produce zero commands!
        p = Policy()
        p.changed(ListenState(busy=True), ListenState(busy=False), 0.0)
        cmds = p.heard(transcript, ListenState(), 0.5, 1.0)
        ok = rc == 0 and len(emitted) == 0 and cmds == []
        record(
            f"vad_whisper_silence_{cid}",
            "pass" if ok else "fail",
            "vad_whisper_policy",
            "synthetic",
            {
                "vad_clips_emitted": len(emitted),
                "direct_whisper_transcript": transcript,
                "decode_ms": elapsed_ms,
                "followup_policy_commands": cmds,
            },
        )

    # ------------------------------------------------------------------
    # 4. VAD + Real Whisper on padded human speech, 700-frame ambient RMS 600
    #    recovery, and 12s saturation bound
    # ------------------------------------------------------------------
    # 4a. Padded real human followup
    padded_clip = clips_by_id["padded_human_voices_followup"]
    padded_samples = read_wav_samples(Path(padded_clip["path"]))
    vad = Vad()
    emitted = [
        c
        for idx in range(0, len(padded_samples) - FRAME + 1, FRAME)
        if (c := vad.feed(padded_samples[idx : idx + FRAME], idx / RATE, False)) is not None
    ]
    transcript, elapsed_ms, rc = ("", 0.0, -1)
    if emitted:
        transcript, elapsed_ms, rc = run_whisper_cli(whisper_bin, whisper_model, emitted[0].pcm)
    norm = normalize_text(transcript)
    ok = rc == 0 and len(emitted) == 1 and all(w in norm for w in ("curiosity", "beside", "moment"))
    record(
        "vad_whisper_padded_human_followup",
        "pass" if ok else "fail",
        "vad_whisper_human",
        "human_recorded",
        {
            "vad_clips_emitted": len(emitted),
            "clip_duration_s": round(len(emitted[0].pcm) / RATE, 3) if emitted else 0.0,
            "transcript": transcript,
            "decode_ms": elapsed_ms,
        },
    )

    # 4b. 700 frames sustained ambient RMS ~600 followed by REAL human speech (voices_human.wav)
    amb_clip = clips_by_id["synth_ambient_600_then_human"]
    amb_samples = read_wav_samples(Path(amb_clip["path"]))
    vad = Vad()
    emitted = [
        c
        for idx in range(0, len(amb_samples) - FRAME + 1, FRAME)
        if (c := vad.feed(amb_samples[idx : idx + FRAME], idx / RATE, False)) is not None
    ]
    # Expect: 1 initial 12s saturation clip from the 700 frames of ambient 600, then noise floor
    # adapts and VAD recovers to capture the subsequent real human speech!
    speech_clip = emitted[-1] if len(emitted) >= 2 else None
    transcript, elapsed_ms, rc = ("", 0.0, -1)
    if speech_clip is not None:
        transcript, elapsed_ms, rc = run_whisper_cli(whisper_bin, whisper_model, speech_clip.pcm)
    norm = normalize_text(transcript)
    ok = (
        len(emitted) >= 2
        and len(emitted[0].pcm) == MAX_FRAMES * FRAME
        and rc == 0
        and any(w in norm for w in ("curiosity", "beside", "moment"))
    )
    record(
        "vad_ambient_600_recovery_then_human_whisper",
        "pass" if ok else "fail",
        "vad_whisper_human",
        "human_recorded_in_synthetic_noise",
        {
            "vad_clips_emitted": len(emitted),
            "first_clip_frames": len(emitted[0].pcm) // FRAME if emitted else 0,
            "recovered_speech_frames": len(speech_clip.pcm) // FRAME if speech_clip else 0,
            "final_noise_power": vad.noise_power,
            "recovered_transcript": transcript,
            "decode_ms": elapsed_ms,
        },
    )

    # 4c. 14s continuous voiced speech hits MAX_FRAMES=600 (12s) saturation and suppresses tail
    trunc_clip = clips_by_id["synth_continuous_14s_truncation"]
    trunc_samples = read_wav_samples(Path(trunc_clip["path"]))
    vad = Vad()
    emitted = [
        c
        for idx in range(0, len(trunc_samples) - FRAME + 1, FRAME)
        if (c := vad.feed(trunc_samples[idx : idx + FRAME], idx / RATE, False)) is not None
    ]
    ok = len(emitted) == 1 and len(emitted[0].pcm) == MAX_FRAMES * FRAME
    record(
        "vad_12s_saturation_bound",
        "pass" if ok else "fail",
        "vad",
        "synthetic",
        {
            "vad_clips_emitted": len(emitted),
            "clip_frames": [len(c.pcm) // FRAME for c in emitted],
            "expected_max_frames": MAX_FRAMES,
        },
    )

    # ------------------------------------------------------------------
    # 5. Real ARM64 ONNX Runtime + openWakeWord + RO private hey_pip.onnx
    # ------------------------------------------------------------------
    if not hey_pip_present:
        record(
            "onnx_neural_wake_suite",
            "missing_evidence",
            "onnx_neural_wake",
            "missing",
            {
                "reason": "Private hey_pip.onnx not mounted read-only; mount via PIP_E2E_HEY_PIP_ONNX.",
                "expected_path": str(hey_pip_path),
            },
        )
    else:
        with tempfile.TemporaryDirectory(prefix="pip-wake-dir-") as tmp_wake:
            wake_dir = Path(tmp_wake)
            (wake_dir / "melspectrogram.onnx").symlink_to(mel_model)
            (wake_dir / "embedding_model.onnx").symlink_to(emb_model)
            (wake_dir / "hey_pip.onnx").symlink_to(hey_pip_path)

            # 5a. Silence warmup & zero false triggers
            sil_res = run_ort_probe(ort_probe_bin, ort_dylib, wake_dir, Path(clips_by_id["synth_wake_silence_16s"]["pcm_path"]))
            ok = (
                sil_res["ort_api_version"] == 20
                and sil_res["warmup_nonzero_hops"] == 0
                and not sil_res["fired"]
                and sil_res["score_max"] < 0.5
                and sil_res["rtf"] < 1.0
            )
            record(
                "onnx_wake_silence_warmup_and_abi",
                "pass" if ok else "fail",
                "onnx_neural_wake",
                "synthetic",
                sil_res,
            )

            # 5b. Real human negative speech rejection (idle threshold 0.5 & busy threshold 0.8)
            for cid in ("human_jfk_negative", "human_voices_followup", "human_librispeech_negative"):
                clip = clips_by_id[cid]
                idle_res = run_ort_probe(ort_probe_bin, ort_dylib, wake_dir, Path(clip["pcm_path"]), busy=False)
                busy_res = run_ort_probe(ort_probe_bin, ort_dylib, wake_dir, Path(clip["pcm_path"]), busy=True)
                ok = (not idle_res["fired"]) and (not busy_res["fired"]) and idle_res["warmup_nonzero_hops"] == 0
                record(
                    f"onnx_wake_human_negative_{cid}",
                    "pass" if ok else "fail",
                    "onnx_neural_wake",
                    "human_recorded",
                    {
                        "clip_id": cid,
                        "wav_sha256": clip["sha256"],
                        "idle": idle_res,
                        "busy": busy_res,
                    },
                )

            # 5c. Sustained ambient RMS 600 + human speech negative rejection
            amb_res = run_ort_probe(
                ort_probe_bin,
                ort_dylib,
                wake_dir,
                Path(clips_by_id["synth_ambient_600_then_human"]["pcm_path"]),
                busy=False,
            )
            record(
                "onnx_wake_ambient_600_negative",
                "pass" if not amb_res["fired"] else "fail",
                "onnx_neural_wake",
                "human_recorded_in_synthetic_noise",
                amb_res,
            )

            # 5d. Busy spoken playback echo negative rejection at busy_threshold=0.8
            echo_res = run_ort_probe(
                ort_probe_bin,
                ort_dylib,
                wake_dir,
                Path(clips_by_id["synth_playback_echo"]["pcm_path"]),
                busy=True,
            )
            record(
                "onnx_wake_busy_echo_rejection",
                "pass" if not echo_res["fired"] else "fail",
                "onnx_neural_wake",
                "human_recorded_attenuated_echo",
                echo_res,
            )

            # 5d-diag. Pure dual-sine 440+880 Hz tone diagnostic (documents non-speech harmonic response of hey_pip.onnx)
            if "synth_pure_tone_440_880" in clips_by_id:
                tone_res = run_ort_probe(
                    ort_probe_bin,
                    ort_dylib,
                    wake_dir,
                    Path(clips_by_id["synth_pure_tone_440_880"]["pcm_path"]),
                    busy=True,
                )
                record(
                    "onnx_wake_pure_tone_diagnostic",
                    "diagnostic_only",
                    "onnx_neural_wake",
                    "synthetic",
                    {
                        "note": "Diagnostic only: pure 440+880 Hz harmonic sine waves are out-of-distribution for speech embeddings and can spike hey_pip.onnx; real room chime/tone testing is required on physical hardware.",
                        **tone_res,
                    },
                )

            # 5e. Human positive wake (opt-in) OR explicit missing_evidence
            pos_clip = clips_by_id.get("human_wake_positive")
            if pos_clip and pos_clip["provenance"] == "human_recorded":
                pos_res = run_ort_probe(ort_probe_bin, ort_dylib, wake_dir, Path(pos_clip["pcm_path"]), busy=False)
                record(
                    "onnx_wake_human_positive",
                    "pass" if pos_res["fired"] else "fail",
                    "onnx_neural_wake",
                    "human_recorded",
                    pos_res,
                )
            else:
                record(
                    "onnx_wake_human_positive",
                    "missing_evidence",
                    "onnx_neural_wake",
                    "missing",
                    {
                        "reason": "No prerecorded human 'Hey Pip' positive clip available; TTS is NOT represented as human wake evidence.",
                        "opt_in_env": "PIP_E2E_HUMAN_WAKE_POSITIVE_WAV",
                    },
                )

            # 5f. Reference score parity (opt-in) OR explicit missing_evidence
            ref_info = manifest["reference_parity_opt_in"]
            if ref_info["present"]:
                ref_res = run_ort_probe(
                    ort_probe_bin,
                    ort_dylib,
                    wake_dir,
                    Path(ref_info["pcm_path"]),
                    ref_scores=Path(ref_info["scores_path"]),
                )
                record(
                    "onnx_wake_reference_score_parity",
                    "pass" if ref_res["reference_parity"]["passed"] else "fail",
                    "onnx_neural_wake",
                    "human_recorded",
                    ref_res,
                )
            else:
                record(
                    "onnx_wake_reference_score_parity",
                    "missing_evidence",
                    "onnx_neural_wake",
                    "missing",
                    {
                        "reason": "No openWakeWord reference PCM + score file supplied.",
                        "opt_in_env": "PIP_E2E_HUMAN_WAKE_PCM + PIP_E2E_HUMAN_WAKE_SCORES",
                    },
                )

            # 5g. Synthetic TTS "Hey Pip" diagnostic ONLY (never passes/fails human wake gate!)
            if "synth_tts_hey_pip_only" in clips_by_id:
                # Pad to >4s so warmup completes before utterance
                raw_tts = read_wav_samples(Path(clips_by_id["synth_tts_hey_pip_only"]["path"]))
                padded_diag = [0] * (RATE * 2) + raw_tts + [0] * (RATE * 2)
                diag_pcm = wake_dir / "diag_tts_hey_pip.pcm"
                diag_pcm.write_bytes(struct.pack(f"<{len(padded_diag)}h", *padded_diag))
                diag_res = run_ort_probe(ort_probe_bin, ort_dylib, wake_dir, diag_pcm, busy=False)
                record(
                    "onnx_wake_synthetic_tts_diagnostic",
                    "diagnostic_only",
                    "onnx_neural_wake",
                    "synthetic",
                    {
                        "note": "Diagnostic only on synthetic espeak-ng TTS (robotic formant voice differs from human speech; never used as human wake acceptance).",
                        **diag_res,
                    },
                )

    # ------------------------------------------------------------------
    # 6. Echo rejection + post-playback human follow-up (Transcript mode)
    # ------------------------------------------------------------------
    # Simulates apps/robot/src/listen/mod.rs behavior:
    # - Transcript mode (wake=None, even with BUDDY_LISTEN_BARGE_IN=1): barge_in is forced FALSE.
    # - During busy=True playback, echo audio NEVER invokes whisper-cli.
    # - When busy transitions True -> False at t=10.0s, Policy opens window [10.35s, 14.35s].
    # - Residual echo onset at 10.10s is rejected by ECHO_TAIL=350ms.
    # - Real human speech (voices_human.wav) starting at 10.50s IS decoded by real whisper-cli -> Control::Say!
    echo_policy = Policy()
    echo_policy.changed(ListenState(busy=True), ListenState(busy=False), 10.0)
    whisper_calls_during_busy = 0
    # Busy playback frame gating check (from mod.rs line 440: if state.busy && !state.confirming { continue; })
    busy_state = ListenState(busy=True)
    if not busy_state.busy:
        whisper_calls_during_busy += 1
    # Residual echo arriving inside ECHO_TAIL (onset 10.10s < 10.35s)
    residual_cmds = echo_policy.heard("playback echo words", ListenState(), 10.10, 10.40)
    # Real human follow-up arriving after ECHO_TAIL (onset 10.50s)
    followup_samples = read_wav_samples(Path(clips_by_id["human_voices_followup"]["path"]))
    followup_text, followup_ms, followup_rc = run_whisper_cli(whisper_bin, whisper_model, followup_samples)
    followup_cmds = echo_policy.heard(followup_text, ListenState(), 10.50, 10.50 + followup_ms / 1000.0)
    echo_ok = (
        whisper_calls_during_busy == 0
        and residual_cmds == []
        and followup_rc == 0
        and len(followup_cmds) == 1
        and followup_cmds[0]["control"] == "Say"
        and "curiosity" in normalize_text(followup_cmds[0]["value"])
    )
    record(
        "echo_suppression_and_post_playback_human_followup",
        "pass" if echo_ok else "fail",
        "echo",
        "human_recorded",
        {
            "whisper_calls_during_busy": whisper_calls_during_busy,
            "echo_tail_rejected_commands": residual_cmds,
            "post_echo_human_transcript": followup_text,
            "post_echo_decode_ms": followup_ms,
            "post_echo_commands": followup_cmds,
        },
    )

    # ------------------------------------------------------------------
    # 7. Slow real Whisper tests:
    #    (a) Slow decode (>1.5s delay) still honors onset-based follow-up window
    #    (b) Held slow decode + full 1-slot clip queue interrupted by priority wake
    #        cancels child cleanly with zero orphan whisper-cli processes
    # ------------------------------------------------------------------
    with tempfile.TemporaryDirectory(prefix="pip-slow-whisper-") as tmp_sw:
        tmp_dir = Path(tmp_sw)

        # 7a. Delayed real whisper-cli (+1500ms) completes & honors onset time even when finish > FOLLOW_S
        delayed_wrapper = tmp_dir / "whisper-delayed"
        make_slow_whisper_wrapper(delayed_wrapper, whisper_bin, delay_ms=1500)
        slow_text, slow_ms, slow_rc = run_whisper_cli(delayed_wrapper, whisper_model, followup_samples)
        slow_policy = Policy()
        slow_policy.changed(ListenState(busy=True), ListenState(busy=False), 10.0)
        # Onset at 10.50s (inside [10.35, 14.35]), decoder finishes at 16.00s (> 14.35s!)
        slow_cmds = slow_policy.heard(slow_text, ListenState(), 10.50, 16.00)
        slow_ok = (
            slow_rc == 0
            and slow_ms >= 1500.0
            and len(slow_cmds) == 1
            and slow_cmds[0]["control"] == "Say"
            and "curiosity" in normalize_text(slow_cmds[0]["value"])
        )
        record(
            "slow_real_whisper_onset_followup",
            "pass" if slow_ok else "fail",
            "slow_whisper",
            "human_recorded",
            {
                "injected_delay_ms": 1500,
                "actual_elapsed_ms": slow_ms,
                "transcript": slow_text,
                "onset_s": 10.50,
                "finish_s": 16.00,
                "policy_commands": slow_cmds,
            },
        )

        # 7b. Held real whisper-cli + full 1-slot clip queue interrupted by priority wake -> clean SIGKILL, 0 orphans
        hold_flag = tmp_dir / "hold.flag"
        started_flag = tmp_dir / "started.pid"
        hold_flag.write_text("1", encoding="utf-8")
        held_wrapper = tmp_dir / "whisper-held"
        make_slow_whisper_wrapper(held_wrapper, whisper_bin, hold_flag=hold_flag, started_flag=started_flag)

        wav_bytes = build_riff_wav(followup_samples)
        held_proc = subprocess.Popen(
            [str(held_wrapper), "-m", str(whisper_model), "-l", "en", "-t", "2", "-np", "-nt", "-f", "-"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert held_proc.stdin is not None
        # Feed stdin asynchronously so pipe never blocks
        def feed_stdin() -> None:
            try:
                held_proc.stdin.write(wav_bytes)
                held_proc.stdin.close()
            except BrokenPipeError:
                pass

        feeder = threading.Thread(target=feed_stdin, daemon=True)
        feeder.start()

        # Wait until slow wrapper is actively holding
        deadline = time.monotonic() + 5.0
        while not started_flag.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        started_ok = started_flag.exists() and held_proc.poll() is None

        # Simulate priority wake interrupting held decode (matches tokio::select! dropping child with kill_on_drop)
        t_cancel = time.monotonic()
        held_proc.kill()
        held_proc.wait(timeout=5.0)
        cancel_ms = (time.monotonic() - t_cancel) * 1000.0
        feeder.join(timeout=2.0)
        hold_flag.unlink(missing_ok=True)
        time.sleep(0.1)
        orphans = count_live_whisper_procs(whisper_bin)

        # Immediately decode a fresh post-wake real human clip using real whisper-cli
        fresh_text, fresh_ms, fresh_rc = run_whisper_cli(whisper_bin, whisper_model, followup_samples)
        cancel_ok = (
            started_ok
            and held_proc.returncode is not None
            and cancel_ms < 1000.0
            and orphans == 0
            and fresh_rc == 0
            and "curiosity" in normalize_text(fresh_text)
        )
        record(
            "slow_real_whisper_priority_wake_cancel_no_orphans",
            "pass" if cancel_ok else "fail",
            "slow_whisper",
            "human_recorded",
            {
                "held_started": started_ok,
                "cancel_latency_ms": round(cancel_ms, 2),
                "orphan_whisper_cli_procs": orphans,
                "post_wake_fresh_transcript": fresh_text,
                "post_wake_decode_ms": fresh_ms,
            },
        )

    # ------------------------------------------------------------------
    # 8. Optional live integration check with tests/pip-e2e/peripherals/daemon.py
    # ------------------------------------------------------------------
    periph_daemon = REPO_ROOT / "tests" / "pip-e2e" / "peripherals" / "daemon.py"
    if periph_daemon.is_file():
        with tempfile.TemporaryDirectory(prefix="pip-speech-periph-") as tmp_p:
            run_dir = Path(tmp_p)
            ready_file = run_dir / "ready.json"
            daemon_proc = subprocess.Popen(
                [
                    "python3",
                    str(periph_daemon),
                    "--run-dir",
                    str(run_dir),
                    "--real-whisper",
                    str(whisper_bin),
                    "--ready-file",
                    str(ready_file),
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                deadline = time.monotonic() + 10.0
                while not ready_file.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
                if not ready_file.exists():
                    raise RuntimeError(
                        f"peripherals daemon did not become ready: {daemon_proc.stderr.read().decode(errors='replace') if daemon_proc.stderr else ''}"
                    )
                ready = json.loads(ready_file.read_text(encoding="utf-8"))
                ctl_sock = Path(ready["control_sock"])
                bin_dir = Path(ready["bin_dir"])
                whisper_slow_bin = bin_dir / "whisper-slow"
                arecord_bin = bin_dir / "arecord"

                # 8a. Stream real human WAV through peripherals fake arecord -> VAD -> real whisper-slow (delayed 500ms)
                jsonrpc_call(
                    ctl_sock,
                    "whisper.configure",
                    {"real_whisper_bin": str(whisper_bin), "delay_before_ms": 500, "hold": False},
                )
                arecord_proc = subprocess.Popen(
                    [str(arecord_bin), "-q", "-D", "default", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                try:
                    jsonrpc_call(
                        ctl_sock,
                        "mic.play_file",
                        {
                            "path": clips_by_id["human_voices_followup"]["path"],
                            "pad_silence_before_ms": 200,
                            "pad_silence_after_ms": 1000,
                        },
                    )
                    # Read frames from fake arecord into real Vad until 1 clip emits
                    vad = Vad()
                    emitted_clip: VadClip | None = None
                    frame_bytes = FRAME * 2
                    t_read_end = time.monotonic() + 12.0
                    assert arecord_proc.stdout is not None
                    while emitted_clip is None and time.monotonic() < t_read_end:
                        raw = arecord_proc.stdout.read(frame_bytes)
                        if len(raw) < frame_bytes:
                            break
                        frame = list(struct.unpack(f"<{FRAME}h", raw))
                        emitted_clip = vad.feed(frame, time.monotonic(), False)
                finally:
                    arecord_proc.kill()
                    arecord_proc.wait(timeout=3.0)

                if emitted_clip is None:
                    raise RuntimeError("VAD did not capture clip from peripherals fake arecord")

                p_text, p_ms, p_rc = run_whisper_cli(whisper_slow_bin, whisper_model, emitted_clip.pcm)
                w_status = jsonrpc_call(ctl_sock, "whisper.status")
                ok = p_rc == 0 and p_ms >= 500.0 and "curiosity" in normalize_text(p_text)
                record(
                    "peripherals_ipc_arecord_vad_and_slow_real_whisper",
                    "pass" if ok else "fail",
                    "peripherals_integration",
                    "human_recorded",
                    {
                        "arecord_bin": str(arecord_bin),
                        "whisper_slow_bin": str(whisper_slow_bin),
                        "vad_clip_duration_s": round(len(emitted_clip.pcm) / RATE, 3),
                        "transcript": p_text,
                        "decode_ms": p_ms,
                        "whisper_status": w_status,
                    },
                )
            except Exception as exc:
                record(
                    "peripherals_ipc_arecord_vad_and_slow_real_whisper",
                    "fail",
                    "peripherals_integration",
                    "human_recorded",
                    {"error": str(exc)},
                )
            finally:
                daemon_proc.terminate()
                try:
                    daemon_proc.wait(timeout=5.0)
                except subprocess.TimeoutExpired:
                    daemon_proc.kill()
    else:
        record(
            "peripherals_ipc_arecord_vad_and_slow_real_whisper",
            "missing_evidence",
            "peripherals_integration",
            "missing",
            {"reason": "tests/pip-e2e/peripherals/daemon.py not present yet; standalone slow-whisper and echo tests ran above."},
        )

    # ------------------------------------------------------------------
    # Assemble final JSON qualification report
    # ------------------------------------------------------------------
    counts = {
        "pass": sum(1 for r in results if r["status"] == "pass"),
        "fail": sum(1 for r in results if r["status"] == "fail"),
        "missing_evidence": sum(1 for r in results if r["status"] == "missing_evidence"),
        "diagnostic_only": sum(1 for r in results if r["status"] == "diagnostic_only"),
    }

    report = {
        "schema_version": 1,
        "suite": "pip-e2e-arm64-speech-qualification",
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "environment": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": platform.python_version(),
        },
        "summary": counts,
        "reliance": {
            "real_arm64_binaries_and_models": [
                "whisper-cli (ggml-org/whisper.cpp v1.9.6, static ARM64 Release)",
                "ggml-tiny.en.bin (Whisper tiny.en English model)",
                "libonnxruntime.so (Microsoft ONNX Runtime 1.20.1 linux-arm64, ORT_API_VERSION=20)",
                "openWakeWord v0.5.1 melspectrogram.onnx + embedding_model.onnx",
                "Private hey_pip.onnx (read-only mounted from local clone when present; never committed/distributed)",
            ],
            "human_recorded_fixtures": [
                c["id"] for c in manifest["clips"] if c.get("provenance") == "human_recorded"
            ],
            "synthetic_fixtures_not_human_evidence": [
                c["id"] for c in manifest["clips"] if c.get("provenance") == "synthetic"
            ],
            "missing_human_evidence": manifest["missing_human_evidence"],
            "hardware_not_qualified_by_this_suite": [
                "Physical reSpeaker USB microphone array beamforming & room acoustics",
                "Physical MAX98357A I2S speaker acoustic coupling / real-room echo cancellation",
                "Human positive 'Hey Pip' wake recall/FAR across speakers unless PIP_E2E_HUMAN_WAKE_POSITIVE_WAV is supplied",
                "Raspberry Pi 5 thermal throttling and Cortex-A76 sustained CPU/memory headroom",
            ],
        },
        "artifacts_sha256": artifacts_hashes,
        "manifest_path": str(manifest_path),
        "results": results,
    }

    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"\n[speech-qualification] report written to {report_path}")
    print(f"[speech-qualification] summary: {json.dumps(counts)}")
    return 1 if counts["fail"] > 0 else 0


if __name__ == "__main__":
    sys.exit(main())
