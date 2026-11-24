#!/usr/bin/env python3
"""
Deterministic provenance materializer for prerecorded human wake fixtures.

Downloads pinned upstream public-domain / CC-licensed human speech recordings
into gitignored <cache>/speech/human-wake-positive/upstream/, verifies SHA-256
digests against recipe.lock.json, decodes via libsndfile1 (present in both
robot-buddy-pip-speech:arm64 and robot-buddy-pip-e2e:arm64), and writes
16kHz mono S16_LE WAV/PCM outputs plus manifest.json.

PROVENANCE DISCIPLINE:
- natural_human_transcript_wake_pip.wav is an unedited Public Domain human
  utterance ("Pip, what a fool you are.") for Whisper transcript wake (idle).
- edited_spliced_human_hey_pip.wav and edited_spliced_human_hey_pip_command.wav
  are explicitly tagged provenance="edited_spliced_human" (component-level
  testing ONLY, NOT a natural held-out recording).
- Never commit generated WAV/PCM files or private ONNX models to git.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import math
import struct
import sys
import urllib.request
import wave
from pathlib import Path
from typing import Any

RATE = 16_000
USER_AGENT = "robot-buddy-pip-e2e-speech-recipe/1.0 (https://github.com/brittianwarner/robot-buddy)"


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


class _SF_INFO(ctypes.Structure):
    _fields_ = [
        ("frames", ctypes.c_int64),
        ("samplerate", ctypes.c_int),
        ("channels", ctypes.c_int),
        ("format", ctypes.c_int),
        ("sections", ctypes.c_int),
        ("seekable", ctypes.c_int),
    ]


def _load_libsndfile() -> ctypes.CDLL:
    for name in ("libsndfile.so.1", "libsndfile.dylib", "libsndfile.1.dylib"):
        try:
            sf = ctypes.CDLL(name)
            sf.sf_open.argtypes = [ctypes.c_char_p, ctypes.c_int, ctypes.POINTER(_SF_INFO)]
            sf.sf_open.restype = ctypes.c_void_p
            sf.sf_seek.argtypes = [ctypes.c_void_p, ctypes.c_int64, ctypes.c_int]
            sf.sf_seek.restype = ctypes.c_int64
            sf.sf_readf_short.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_int16), ctypes.c_int64]
            sf.sf_readf_short.restype = ctypes.c_int64
            sf.sf_close.argtypes = [ctypes.c_void_p]
            sf.sf_close.restype = ctypes.c_int
            return sf
        except OSError:
            continue
    raise RuntimeError(
        "libsndfile.so.1 not found. Run this script inside the locked ARM64 "
        "robot-buddy-pip-speech:arm64 or robot-buddy-pip-e2e:arm64 container."
    )


def read_slice_16k(
    sf: ctypes.CDLL,
    path: Path,
    start_s: float = 0.0,
    duration_s: float | None = None,
) -> list[int]:
    info = _SF_INFO()
    handle = sf.sf_open(str(path).encode("utf-8"), 0x10, ctypes.byref(info))  # SFM_READ
    if not handle:
        raise RuntimeError(f"libsndfile failed to open {path}")
    try:
        sr = info.samplerate
        ch = info.channels
        if sr <= 0 or ch <= 0:
            raise RuntimeError(f"Invalid audio stream metadata in {path}: sr={sr}, ch={ch}")
        start_f = int(round(start_s * sr))
        if start_f > 0:
            sf.sf_seek(handle, start_f, 0)  # SEEK_SET
        remaining = max(0, info.frames - start_f)
        want_f = remaining if duration_s is None else min(remaining, int(round(duration_s * sr)))
        buf = (ctypes.c_int16 * (want_f * ch))()
        got = sf.sf_readf_short(handle, buf, want_f)
        if ch == 1:
            mono = [int(buf[i]) for i in range(got)]
        else:
            mono = [
                sum(int(buf[i * ch + c]) for c in range(ch)) // ch
                for i in range(got)
            ]
        if sr == RATE or not mono:
            return mono
        out_len = max(1, int(round(len(mono) * RATE / sr)))
        scale = (len(mono) - 1) / max(1, out_len - 1)
        out = [0] * out_len
        for i in range(out_len):
            pos = i * scale
            lo = int(pos)
            hi = min(lo + 1, len(mono) - 1)
            frac = pos - lo
            val = mono[lo] * (1.0 - frac) + mono[hi] * frac
            out[i] = max(-32768, min(32767, int(round(val))))
        return out
    finally:
        sf.sf_close(handle)


def fetch_verified(src: dict[str, Any], upstream_dir: Path, cache_dir: Path) -> Path:
    dest = upstream_dir / src["filename"]
    expected = src["sha256"]

    if dest.is_file() and sha256_file(dest) == expected:
        return dest

    # Reuse already-cached VOiCES clip from prepare-fixtures.py if present and matching SHA-256
    if src["filename"] == "Lab41-SRI-VOiCES-src-sp0307-ch127535-sg0042.wav":
        existing = cache_dir / "speech" / "fixtures" / "human" / "human_voices_followup.wav"
        if existing.is_file() and sha256_file(existing) == expected:
            dest.write_bytes(existing.read_bytes())
            return dest

    tmp = dest.with_suffix(dest.suffix + ".tmp")
    req = urllib.request.Request(src["url"], headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=120) as resp:
        data = resp.read()
    tmp.write_bytes(data)
    got = sha256_file(tmp)
    if got != expected:
        tmp.unlink(missing_ok=True)
        raise RuntimeError(
            f"SHA-256 mismatch for {src['filename']}: expected {expected}, got {got}"
        )
    tmp.replace(dest)
    return dest


def write_wav_and_pcm(wav_path: Path, pcm_path: Path, samples: list[int]) -> dict[str, Any]:
    raw = struct.pack(f"<{len(samples)}h", *samples)
    pcm_path.write_bytes(raw)
    with wave.open(str(wav_path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(raw)
    rms = math.sqrt(sum(s * s for s in samples) / max(1, len(samples)))
    peak = max((abs(s) for s in samples), default=0)
    return {
        "samples": len(samples),
        "duration_s": round(len(samples) / RATE, 3),
        "rms": round(rms, 2),
        "peak": peak,
        "wav_sha256": sha256_file(wav_path),
        "pcm_sha256": sha256_file(pcm_path),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Materialize verified human wake positive clips into .cache/")
    parser.add_argument(
        "--cache",
        type=Path,
        default=Path("/opt/pip-e2e/cache"),
        help="Gitignored cache root (default: /opt/pip-e2e/cache)",
    )
    parser.add_argument(
        "--lockfile",
        type=Path,
        default=Path(__file__).resolve().parent / "recipe.lock.json",
        help="Path to recipe.lock.json",
    )
    parser.add_argument(
        "--verify-output-hashes",
        action="store_true",
        help="Fail if generated WAV SHA-256 digests differ from observed_arm64 in recipe.lock.json",
    )
    args = parser.parse_args()

    lock = json.loads(args.lockfile.read_text(encoding="utf-8"))
    out_dir = args.cache / "speech" / "human-wake-positive"
    upstream_dir = out_dir / "upstream"
    upstream_dir.mkdir(parents=True, exist_ok=True)

    sf = _load_libsndfile()

    # 1. Fetch and verify all pinned upstream recordings
    src_paths: dict[str, Path] = {}
    for key, src in lock["upstream_sources"].items():
        p = fetch_verified(src, upstream_dir, args.cache)
        src_paths[key] = p
        print(f"[human-wake-recipe] verified upstream {key}: {p.name} ({src['sha256'][:12]}...)")

    # 2. Slice exact boundaries (no signal tuning / EQ / gain / pitch modifications)
    params = lock["assembly_parameters"]
    slices = params["slices_s"]
    lead = [0] * int(round(RATE * params["lead_silence_s"]))
    trail = [0] * int(round(RATE * params["trail_silence_s"]))
    gap_word = [0] * int(round(RATE * params["inter_word_gap_s"]))
    gap_cmd = [0] * int(round(RATE * params["command_pause_s"]))

    sl_nat = slices["librivox_ch17_pip_what_a_fool"]
    nat_pip = read_slice_16k(sf, src_paths[sl_nat["source"]], sl_nat["start_s"], sl_nat["duration_s"])

    sl_hey = slices["dvortygirl_hey"]
    d_hey = read_slice_16k(sf, src_paths[sl_hey["source"]], sl_hey["start_s"], sl_hey["duration_s"])

    sl_pip = slices["dvortygirl_pip"]
    d_pip = read_slice_16k(sf, src_paths[sl_pip["source"]], sl_pip["start_s"], sl_pip["duration_s"])

    sl_voices = slices["voices_followup_trimmed"]
    voices = read_slice_16k(sf, src_paths[sl_voices["source"]])
    v_thresh = int(sl_voices["trim_abs_amplitude_threshold"])
    v_pad = int(sl_voices["trim_pad_samples"])
    v_start = next((i for i, s in enumerate(voices) if abs(s) >= v_thresh), 0)
    v_end = next((i for i in range(len(voices) - 1, -1, -1) if abs(voices[i]) >= v_thresh), len(voices) - 1)
    voices_trimmed = voices[max(0, v_start - v_pad) : min(len(voices), v_end + v_pad)]

    assembled: dict[str, list[int]] = {
        "natural_human_transcript_wake_pip": lead + nat_pip + trail,
        "edited_spliced_human_hey_pip": lead + d_hey + gap_word + d_pip + trail,
        "edited_spliced_human_hey_pip_command": lead + d_hey + gap_word + d_pip + gap_cmd + voices_trimmed + trail,
        "control_human_hey_only": lead + d_hey + trail,
        "control_human_pip_only": lead + d_pip + trail,
        "control_human_pip_hey_reversed": lead + d_pip + gap_word + d_hey + trail,
    }

    manifest_clips: list[dict[str, Any]] = []
    mismatches: list[str] = []

    for entry in lock["outputs"]:
        cid = entry["id"]
        samples = assembled[cid]
        wav_path = out_dir / entry["filename"]
        pcm_path = out_dir / (Path(entry["filename"]).stem + ".pcm")
        stats = write_wav_and_pcm(wav_path, pcm_path, samples)
        expected_sha = entry.get("observed_arm64", {}).get("wav_sha256")
        if args.verify_output_hashes and expected_sha and stats["wav_sha256"] != expected_sha:
            mismatches.append(f"{cid}: expected {expected_sha}, got {stats['wav_sha256']}")
        manifest_clips.append(
            {
                "id": cid,
                "wav_path": str(wav_path),
                "pcm_path": str(pcm_path),
                "provenance": entry["provenance"],
                "is_human_recording": entry["is_human_recording"],
                "is_natural_unmodified_utterance": entry["is_natural_unmodified_utterance"],
                "is_edited_spliced": entry["is_edited_spliced"],
                "license_summary": entry["license_summary"],
                "target_path": entry["target_path"],
                "sources": entry["sources"],
                "audio": stats,
            }
        )
        print(
            f"[human-wake-recipe] wrote {wav_path.name} "
            f"({entry['provenance']}, {stats['duration_s']}s, sha256={stats['wav_sha256'][:12]}...)"
        )

    manifest = {
        "schema_version": 1,
        "recipe_lockfile": str(args.lockfile),
        "policy": lock["policy"],
        "clips": manifest_clips,
    }
    manifest_path = out_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"[human-wake-recipe] wrote {manifest_path}")

    if mismatches:
        for m in mismatches:
            print(f"[human-wake-recipe] HASH MISMATCH: {m}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
