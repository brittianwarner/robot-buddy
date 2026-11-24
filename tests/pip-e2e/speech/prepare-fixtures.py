#!/usr/bin/env python3
"""
Fetch and verify prerecorded human speech fixtures, detect optional user-supplied
human recordings, generate clearly-labeled synthetic pipeline fixtures, and emit
<cache>/speech/fixtures/manifest.json.

PROVENANCE DISCIPLINE:
- "human_recorded" is ONLY used for genuine prerecorded human speech (verified
  public human corpora or operator-supplied opt-in recordings).
- Synthetic/TTS clips (espeak-ng or PCM waveforms) are ALWAYS tagged
  provenance="synthetic", is_human_recording=False, and NEVER represented as
  human speech or wake accuracy proof.
- Missing human speech categories (such as prerecorded human "Hey Pip" positive
  wake audio) are explicitly emitted with provenance="missing" and opt-in env vars.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import random
import shutil
import struct
import subprocess
import tarfile
import urllib.request
import wave
from pathlib import Path
from typing import Any

RATE = 16_000
FRAME = 320  # 20 ms at 16 kHz


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def wav_stats(path: Path) -> dict[str, Any]:
    with wave.open(str(path), "rb") as w:
        ch = w.getnchannels()
        sw = w.getsampwidth()
        sr = w.getframerate()
        n = w.getnframes()
        raw = w.readframes(n)
    if ch != 1 or sw != 2 or sr != RATE:
        raise ValueError(f"{path} must be 16kHz mono 16-bit PCM, got ch={ch} sw={sw} sr={sr}")
    samples = struct.unpack(f"<{len(raw) // 2}h", raw) if raw else ()
    rms = math.sqrt(sum(s * s for s in samples) / len(samples)) if samples else 0.0
    peak = max((abs(s) for s in samples), default=0)
    return {
        "channels": ch,
        "sample_width_bytes": sw,
        "sample_rate_hz": sr,
        "samples": n,
        "duration_s": round(n / sr, 3),
        "rms": round(rms, 2),
        "peak": peak,
    }


def write_wav_and_pcm(wav_path: Path, pcm_path: Path, samples: list[int]) -> None:
    wav_path.parent.mkdir(parents=True, exist_ok=True)
    pcm_path.parent.mkdir(parents=True, exist_ok=True)
    clamped = [max(-32768, min(32767, int(s))) for s in samples]
    raw = struct.pack(f"<{len(clamped)}h", *clamped)
    pcm_path.write_bytes(raw)
    with wave.open(str(wav_path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(raw)


def wav_to_pcm(wav_path: Path, pcm_path: Path) -> list[int]:
    with wave.open(str(wav_path), "rb") as w:
        if w.getnchannels() != 1 or w.getsampwidth() != 2 or w.getframerate() != RATE:
            raise ValueError(f"Expected 16kHz mono S16_LE WAV: {wav_path}")
        raw = w.readframes(w.getnframes())
    pcm_path.parent.mkdir(parents=True, exist_ok=True)
    pcm_path.write_bytes(raw)
    return list(struct.unpack(f"<{len(raw) // 2}h", raw))


def linear_resample_to_16k(samples: list[int], src_rate: int) -> list[int]:
    if src_rate == RATE or not samples:
        return samples
    out_len = max(1, int(round(len(samples) * RATE / src_rate)))
    out = [0] * out_len
    scale = (len(samples) - 1) / max(1, out_len - 1)
    for i in range(out_len):
        pos = i * scale
        lo = int(math.floor(pos))
        hi = min(lo + 1, len(samples) - 1)
        frac = pos - lo
        val = samples[lo] * (1.0 - frac) + samples[hi] * frac
        out[i] = max(-32768, min(32767, int(round(val))))
    return out


def synth_espeak_clip(text: str, wav_path: Path, pcm_path: Path, voice: str = "en-us+f3", speed: int = 130) -> bool:
    espeak = shutil.which("espeak-ng") or shutil.which("espeak")
    if not espeak:
        return False
    tmp = wav_path.with_suffix(".espeak.wav")
    subprocess.run(
        [espeak, "-v", voice, "-s", str(speed), "-w", str(tmp), text],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    with wave.open(str(tmp), "rb") as w:
        ch = w.getnchannels()
        sw = w.getsampwidth()
        sr = w.getframerate()
        raw = w.readframes(w.getnframes())
    tmp.unlink(missing_ok=True)
    if sw != 2:
        raise ValueError(f"Unexpected espeak sample width {sw}")
    raw_samples = struct.unpack(f"<{len(raw) // 2}h", raw)
    mono = list(raw_samples[::ch])
    resampled = linear_resample_to_16k(mono, sr)
    # Normalize peak to ~14000 so RMS is comfortably above VAD threshold without clipping
    peak = max((abs(s) for s in resampled), default=1)
    if peak > 0:
        gain = 14000.0 / peak
        resampled = [int(round(s * gain)) for s in resampled]
    # Add 300ms leading silence and 1000ms trailing silence for clean VAD endpointing
    padded = [0] * (RATE * 3 // 10) + resampled + [0] * RATE
    write_wav_and_pcm(wav_path, pcm_path, padded)
    return True


def decode_float32_wav_to_s16le(raw_bytes: bytes) -> list[int]:
    pos = 12
    fmt = None
    pcm_raw = None
    while pos + 8 <= len(raw_bytes):
        cid, sz = struct.unpack_from("<4sI", raw_bytes, pos)
        pos += 8
        if cid == b"fmt ":
            fmt = struct.unpack_from("<HHIIHH", raw_bytes, pos)
        elif cid == b"data":
            pcm_raw = raw_bytes[pos : pos + sz]
        pos += sz + (sz % 2)
    if fmt is None or pcm_raw is None:
        raise ValueError("Malformed float32 WAV")
    tag, ch, sr, _br, _ba, bits = fmt
    if tag != 3 or ch != 1 or sr != RATE or bits != 32:
        raise ValueError(f"Expected 16kHz mono float32 WAV, got tag={tag} ch={ch} sr={sr} bits={bits}")
    floats = struct.unpack(f"<{len(pcm_raw) // 4}f", pcm_raw)
    return [max(-32768, min(32767, int(round(x * 32767.0)))) for x in floats]


def fetch_human_recording(entry: dict[str, Any], cache: Path, lock: dict[str, Any]) -> Path:
    dest = cache / entry["cache_relpath"]
    dest.parent.mkdir(parents=True, exist_ok=True)
    expected = entry["sha256"]
    is_f32 = entry.get("upstream_format") == "ieee_float32_wav"

    if not is_f32 and dest.is_file() and sha256_file(dest) == expected:
        return dest

    raw_dest = dest.with_suffix(".upstream.wav") if is_f32 else dest
    if not (raw_dest.is_file() and sha256_file(raw_dest) == expected):
        source = entry.get("source", "")
        if source.startswith("whisper_cpp_tarball:"):
            member = source.split(":", 1)[1]
            tar_path = cache / lock["runtime"]["whisper_cpp"]["cache_relpath"]
            if not tar_path.is_file():
                raise FileNotFoundError(f"Missing whisper.cpp tarball at {tar_path}")
            with tarfile.open(tar_path, "r:gz") as tf:
                fobj = tf.extractfile(member)
                if fobj is None:
                    raise FileNotFoundError(f"Member {member} not in {tar_path}")
                raw_dest.write_bytes(fobj.read())
        else:
            url = entry["url"]
            tmp = raw_dest.with_suffix(".tmp")
            urllib.request.urlretrieve(url, tmp)
            tmp.replace(raw_dest)

        got = sha256_file(raw_dest)
        if got != expected:
            raw_dest.unlink(missing_ok=True)
            raise RuntimeError(f"SHA-256 mismatch for {entry['id']}: expected {expected}, got {got}")

    if is_f32:
        samples = decode_float32_wav_to_s16le(raw_dest.read_bytes())
        write_wav_and_pcm(dest, dest.with_suffix(".tmp.pcm"), samples)
        dest.with_suffix(".tmp.pcm").unlink(missing_ok=True)

    return dest


def detect_opt_in_human(
    env_var: str,
    fallback_dir: Path | None,
    fallback_name: str,
) -> Path | None:
    explicit = os.environ.get(env_var, "").strip()
    if explicit:
        p = Path(explicit)
        if p.is_file():
            return p
    if fallback_dir is not None:
        candidate = fallback_dir / fallback_name
        if candidate.is_file():
            return candidate
    return None


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cache", required=True, type=Path)
    parser.add_argument("--lockfile", required=True, type=Path)
    args = parser.parse_args()

    cache: Path = args.cache
    lock = json.loads(args.lockfile.read_text(encoding="utf-8"))
    fixtures_dir = cache / "speech" / "fixtures"
    human_dir = fixtures_dir / "human"
    synth_dir = fixtures_dir / "synthetic"
    pcm_dir = fixtures_dir / "pcm"
    human_dir.mkdir(parents=True, exist_ok=True)
    synth_dir.mkdir(parents=True, exist_ok=True)
    pcm_dir.mkdir(parents=True, exist_ok=True)

    clips: list[dict[str, Any]] = []
    human_samples_by_id: dict[str, list[int]] = {}

    # 1. Verified public prerecorded human speech recordings
    for entry in lock["human_recordings"]:
        wav_path = fetch_human_recording(entry, cache, lock)
        pcm_path = pcm_dir / f"{entry['id']}.pcm"
        samples = wav_to_pcm(wav_path, pcm_path)
        human_samples_by_id[entry["id"]] = samples
        stats = wav_stats(wav_path)
        clips.append(
            {
                "id": entry["id"],
                "kind": entry["kind"],
                "text": entry["expected_text"],
                "expected_substrings": entry["expected_substrings"],
                "path": str(wav_path),
                "pcm_path": str(pcm_path),
                "sha256": sha256_file(wav_path),
                "pcm_sha256": sha256_file(pcm_path),
                "provenance": "human_recorded",
                "is_human_recording": True,
                "license": entry["license"],
                "audio": stats,
            }
        )

    # 2. Optional user-supplied prerecorded human clips (read-only opt-in; never copied into git)
    opt_in_dir_raw = os.environ.get("PIP_E2E_HUMAN_SPEECH_DIR", "").strip()
    opt_in_dir = Path(opt_in_dir_raw) if opt_in_dir_raw and Path(opt_in_dir_raw).is_dir() else None

    opt_in_specs = [
        (
            "human_wake_positive",
            "wake_positive",
            "PIP_E2E_HUMAN_WAKE_POSITIVE_WAV",
            "wake_positive.wav",
            "Hey Pip (prerecorded human utterance)",
            "Required to qualify neural hey_pip.onnx positive recall and human wake accuracy.",
        ),
        (
            "human_opt_in_wake_negative",
            "wake_negative",
            "PIP_E2E_HUMAN_WAKE_NEGATIVE_WAV",
            "wake_negative.wav",
            "Optional local human negative speech recording",
            "Optional supplemental human negative clip (public human negatives jfk/voices/ldc93s1 are already present).",
        ),
        (
            "human_opt_in_followup",
            "followup",
            "PIP_E2E_HUMAN_FOLLOWUP_WAV",
            "followup.wav",
            "Optional local human follow-up speech recording",
            "Optional supplemental human follow-up clip (public human followup voices_human is already present).",
        ),
    ]

    missing_evidence: list[dict[str, Any]] = []
    for cid, kind, env_var, fname, desc, note in opt_in_specs:
        found = detect_opt_in_human(env_var, opt_in_dir, fname)
        if found is not None:
            pcm_path = pcm_dir / f"{cid}.pcm"
            wav_to_pcm(found, pcm_path)
            stats = wav_stats(found)
            clips.append(
                {
                    "id": cid,
                    "kind": kind,
                    "text": desc,
                    "path": str(found),
                    "pcm_path": str(pcm_path),
                    "sha256": sha256_file(found),
                    "pcm_sha256": sha256_file(pcm_path),
                    "provenance": "human_recorded",
                    "is_human_recording": True,
                    "opt_in_env": env_var,
                    "read_only_source": True,
                    "audio": stats,
                }
            )
        else:
            item = {
                "id": cid,
                "kind": kind,
                "text": desc,
                "path": None,
                "pcm_path": None,
                "sha256": None,
                "provenance": "missing",
                "is_human_recording": False,
                "opt_in_env": env_var,
                "opt_in_dir_file": f"${{PIP_E2E_HUMAN_SPEECH_DIR}}/{fname}",
                "note": note,
            }
            clips.append(item)
            if cid == "human_wake_positive":
                missing_evidence.append(item)

    # Check optional reference parity files for compare_reference_scores
    ref_pcm = detect_opt_in_human("PIP_E2E_HUMAN_WAKE_PCM", opt_in_dir, "reference_clip.pcm")
    ref_scores = detect_opt_in_human("PIP_E2E_HUMAN_WAKE_SCORES", opt_in_dir, "reference_scores.txt")
    reference_parity_opt_in = {
        "id": "human_wake_reference_parity",
        "present": bool(ref_pcm and ref_scores),
        "pcm_path": str(ref_pcm) if ref_pcm else None,
        "scores_path": str(ref_scores) if ref_scores else None,
        "provenance": "human_recorded" if (ref_pcm and ref_scores) else "missing",
        "opt_in_env": "PIP_E2E_HUMAN_WAKE_PCM + PIP_E2E_HUMAN_WAKE_SCORES",
        "note": "Required for openWakeWord reference score parity (<0.02 abs diff, compare_reference_scores).",
    }
    if not reference_parity_opt_in["present"]:
        missing_evidence.append(reference_parity_opt_in)

    # 3. Deterministic waveform synthetic fixtures (VAD, noise floor, echo, truncation)
    rng = random.Random(20261124)

    def uniform_rms_noise(n_samples: int, target_rms: float) -> list[int]:
        # Uniform[-A, A] has RMS = A / sqrt(3)
        amp = target_rms * math.sqrt(3.0)
        return [int(round(rng.uniform(-amp, amp))) for _ in range(n_samples)]

    # 3a. Pure silence (2.0s)
    silence_wav = synth_dir / "synth_silence.wav"
    silence_pcm = pcm_dir / "synth_silence.pcm"
    write_wav_and_pcm(silence_wav, silence_pcm, [0] * (RATE * 2))

    # 3b. Silence warmup for ONNX neural wake (16.0s = 200 hops)
    wake_silence_wav = synth_dir / "synth_wake_silence_16s.wav"
    wake_silence_pcm = pcm_dir / "synth_wake_silence_16s.pcm"
    write_wav_and_pcm(wake_silence_wav, wake_silence_pcm, [0] * (RATE * 16))

    # 3c. Ambient RMS 450 + single click + trailing silence
    click_samples = (
        uniform_rms_noise(100 * FRAME, 450.0)
        + [32000] * FRAME
        + uniform_rms_noise(60 * FRAME, 300.0)
    )
    click_wav = synth_dir / "synth_ambient_450_click.wav"
    click_pcm = pcm_dir / "synth_ambient_450_click.pcm"
    write_wav_and_pcm(click_wav, click_pcm, click_samples)

    # 3d. Sustained ambient RMS 600 (700 frames = 14s) followed by REAL human speech (voices_human) + 1s ambient
    # Directly tests the >500 RMS ambient noise lockout boundary!
    ambient_600 = uniform_rms_noise(700 * FRAME, 600.0)
    voices_pcm = human_samples_by_id["human_voices_followup"]
    # Mix human speech on top of ambient RMS 600 so transition is realistic
    mixed_tail_noise = uniform_rms_noise(len(voices_pcm) + 60 * FRAME, 600.0)
    ambient_600_then_human = list(ambient_600)
    for i, n in enumerate(mixed_tail_noise):
        s = voices_pcm[i] if i < len(voices_pcm) else 0
        ambient_600_then_human.append(max(-32768, min(32767, n + s)))
    amb600_wav = synth_dir / "synth_ambient_600_then_human.wav"
    amb600_pcm = pcm_dir / "synth_ambient_600_then_human.pcm"
    write_wav_and_pcm(amb600_wav, amb600_pcm, ambient_600_then_human)

    # 3e. Padded human speech (400ms silence + human_voices_followup + 1000ms silence)
    padded_human = [0] * (20 * FRAME) + voices_pcm + [0] * (50 * FRAME)
    padded_human_wav = synth_dir / "padded_human_voices_followup.wav"
    padded_human_pcm = pcm_dir / "padded_human_voices_followup.pcm"
    write_wav_and_pcm(padded_human_wav, padded_human_pcm, padded_human)

    # 3f. 14s continuous loud speech/tone for MAX_FRAMES=600 (12s) truncation test
    # Loop human_jfk_negative to 14.0s with zero long pauses so VAD hits 600-frame saturation
    jfk_pcm = human_samples_by_id["human_jfk_negative"]
    loud_chunks = [s for s in jfk_pcm if abs(s) > 400]
    continuous_14s = []
    while len(continuous_14s) < 700 * FRAME:
        continuous_14s.extend(loud_chunks)
    continuous_14s = continuous_14s[: 700 * FRAME] + [0] * (60 * FRAME)
    trunc_wav = synth_dir / "synth_continuous_14s_truncation.wav"
    trunc_pcm = pcm_dir / "synth_continuous_14s_truncation.pcm"
    write_wav_and_pcm(trunc_wav, trunc_pcm, continuous_14s)

    # 3g. Simulated spoken playback echo clip (attenuated 0.5x gain 3.0s excerpt of human_jfk_negative + 1.0s silence)
    # Represents realistic speaker playback echo when Pip speaks a reply.
    echo_samples = [int(round(s * 0.5)) for s in jfk_pcm[: RATE * 3]] + [0] * (50 * FRAME)
    echo_wav = synth_dir / "synth_playback_echo.wav"
    echo_pcm = pcm_dir / "synth_playback_echo.pcm"
    write_wav_and_pcm(echo_wav, echo_pcm, echo_samples)

    # 3h. Pure dual-sine 440+880 Hz harmonic tone (1.5s) — diagnostic clip for neural wake non-speech tone sensitivity
    tone_samples = [
        int(round(4200.0 * math.sin(2.0 * math.pi * 440.0 * i / RATE)
                  + 1800.0 * math.sin(2.0 * math.pi * 880.0 * i / RATE)))
        for i in range(int(RATE * 1.5))
    ] + [0] * (50 * FRAME)
    tone_wav = synth_dir / "synth_pure_tone_440_880.wav"
    tone_pcm = pcm_dir / "synth_pure_tone_440_880.pcm"
    write_wav_and_pcm(tone_wav, tone_pcm, tone_samples)

    waveform_entries = [
        ("synth_silence", "wake_negative", "", silence_wav, silence_pcm, "Pure 2s digital silence"),
        ("synth_wake_silence_16s", "wake_negative", "", wake_silence_wav, wake_silence_pcm, "16s silence for ONNX warmup & zero-false-trigger verification"),
        ("synth_ambient_450_click", "wake_negative", "", click_wav, click_pcm, "Ambient RMS 450 + single impulse click"),
        ("synth_ambient_600_then_human", "followup", "I had that curiosity beside me at this moment.", amb600_wav, amb600_pcm, "700 frames ambient RMS 600 + real human voices_human.wav speech"),
        ("padded_human_voices_followup", "followup", "I had that curiosity beside me at this moment.", padded_human_wav, padded_human_pcm, "Silence-padded real human voices_human.wav for VAD endpointing"),
        ("synth_continuous_14s_truncation", "command", "", trunc_wav, trunc_pcm, "14s continuous voiced speech for MAX_FRAMES=600 (12s) saturation bound"),
        ("synth_playback_echo", "wake_negative", "", echo_wav, echo_pcm, "Attenuated spoken playback echo (0.5x gain JFK excerpt) for busy/echo-tail rejection"),
        ("synth_pure_tone_440_880", "wake_negative", "", tone_wav, tone_pcm, "Pure 440+880 Hz dual-sine tone (diagnostic for ONNX non-speech harmonic response)"),
    ]
    for cid, kind, text, wp, pp, desc in waveform_entries:
        clips.append(
            {
                "id": cid,
                "kind": kind,
                "text": text,
                "description": desc,
                "path": str(wp),
                "pcm_path": str(pp),
                "sha256": sha256_file(wp),
                "pcm_sha256": sha256_file(pp),
                "provenance": "synthetic",
                "is_human_recording": False,
                "generator": "deterministic_pcm_python",
                "audio": wav_stats(wp),
            }
        )

    # 4. Clearly-labeled synthetic TTS fixtures (ONLY for transcript-parser/policy/pipeline mechanics; NEVER human evidence!)
    tts_specs = [
        ("synth_tts_hey_pip_sleep", "command", "Hey Pip, go to sleep.", ["pip", "sleep"], "en-us+f3", 130),
        ("synth_tts_third_party_pip", "wake_negative", "My name is Pip.", ["name", "pip"], "en-gb", 130),
        ("synth_tts_followup_tomorrow", "followup", "What about tomorrow?", ["tomorrow"], "en-us+f3", 130),
        ("synth_tts_confirm_code", "command", "Confirm zero one two three four five.", ["confirm"], "en-us+f3", 130),
        ("synth_tts_acoustic_misheard_question", "command", "Hey, Pip! What time is it?", ["time"], "en-us+f3", 130),
        ("synth_tts_acoustic_misheard_stop", "command", "Pip, stop talking.", ["stop"], "en-us+f3", 130),
        ("synth_tts_hey_pip_only", "wake_positive", "Hey Pip.", ["pip"], "en-us+f3", 130),
    ]
    espeak_available = bool(shutil.which("espeak-ng") or shutil.which("espeak"))
    for cid, kind, text, substrings, voice, speed in tts_specs:
        wp = synth_dir / f"{cid}.wav"
        pp = pcm_dir / f"{cid}.pcm"
        if synth_espeak_clip(text, wp, pp, voice=voice, speed=speed):
            clips.append(
                {
                    "id": cid,
                    "kind": kind,
                    "text": text,
                    "expected_substrings": substrings,
                    "path": str(wp),
                    "pcm_path": str(pp),
                    "sha256": sha256_file(wp),
                    "pcm_sha256": sha256_file(pp),
                    "provenance": "synthetic",
                    "is_human_recording": False,
                    "generator": f"espeak-ng ({voice}, s={speed})",
                    "warning": "SYNTHETIC TTS ONLY — NOT a human recording; never use to claim human wake accuracy.",
                    "audio": wav_stats(wp),
                }
            )

    manifest = {
        "schema_version": 1,
        "format": "16kHz mono S16_LE WAV + raw PCM",
        "espeak_ng_available": espeak_available,
        "clips": clips,
        "missing_human_evidence": missing_evidence,
        "reference_parity_opt_in": reference_parity_opt_in,
        "opt_in_env_vars": {
            "PIP_E2E_HEY_PIP_ONNX": "Path to private hey_pip.onnx (mounted read-only; never copied into repo/image)",
            "PIP_E2E_HUMAN_SPEECH_DIR": "Optional read-only dir containing wake_positive.wav, wake_negative.wav, followup.wav, reference_clip.pcm, reference_scores.txt",
            "PIP_E2E_HUMAN_WAKE_POSITIVE_WAV": "Optional 16kHz mono S16_LE WAV of a human saying 'Hey Pip'",
            "PIP_E2E_HUMAN_WAKE_NEGATIVE_WAV": "Optional 16kHz mono S16_LE WAV of human non-wake speech",
            "PIP_E2E_HUMAN_FOLLOWUP_WAV": "Optional 16kHz mono S16_LE WAV of human follow-up speech",
            "PIP_E2E_HUMAN_WAKE_PCM": "Optional raw 16kHz mono S16_LE PCM for openWakeWord reference score parity",
            "PIP_E2E_HUMAN_WAKE_SCORES": "Optional newline-delimited reference scores matching PIP_E2E_HUMAN_WAKE_PCM",
        },
    }

    manifest_path = fixtures_dir / "manifest.json"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"[prepare-fixtures] wrote {manifest_path} ({len(clips)} entries, {len(missing_evidence)} missing human evidence items)")


if __name__ == "__main__":
    main()
