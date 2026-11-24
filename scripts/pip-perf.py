#!/usr/bin/env python3
"""On-device Raspberry Pi 5 acoustic benchmark and analysis tool for Pip.

Measures real acoustic latencies, 50 Hz neck deadline misses (distinguishing
late cycles from skipped 20 ms slots), positive camera inference activity, and
systemd cgroup memory headroom under the 1 GiB profile (infra/robot-buddy-pip.conf)
without modifying any repository defaults or exporting private device env values.

Acceptance thresholds enforced by `run` and `compare --strict` are provisional
targets, not measured hardware guarantees.

Subcommands:
  analyze-turn    Measure end-of-speech -> first audible from a 16-bit PCM WAV.
  analyze-barge   Measure wake END -> actual playback stop from a 16-bit PCM WAV.
  parse-journal   Extract 50 Hz neck loop timing, camera inference activity,
                  audio timing, and safety/peripheral faults from journalctl.
  cgroup-snapshot Read current/peak cgroup memory, tasks, restarts, and Pi
                  thermal/throttle state for `robot-buddy.service`.
  run             Execute an on-device acoustic benchmark run with camera and
                  neck active and emit a versioned JSON artifact.
  compare         Compare recorded variant artifacts (baseline, warm-whisper,
                  endpoint-shortening) against provisional acceptance targets.
"""

from __future__ import annotations

import argparse
import json
import math
import platform
import re
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

SCHEMA_VERSION = 2
GIB = 1024 * 1024 * 1024

# Provisional acceptance targets (NOT measured hardware guarantees).
PROVISIONAL_TARGETS: Dict[str, Any] = {
    "label": "provisional_target_not_measured_guarantee",
    "min_turns": 20,
    "min_barge_ins_neural": 10,
    "eos_to_first_audible_p95_max_ms": 3000.0,
    "neural_wake_end_to_playback_stop_p95_max_ms": 500.0,
    "transcript_echo_interruptions_max": 0,
    "max_skipped_slot_rate_pct": 0.1,
    "memory_limit_bytes": GIB,
    "memory_reserve_pct": 10.0,
    "memory_target_max_bytes": int(GIB * 0.90),  # 921.6 MiB (10% reserve below 1 GiB)
    "max_restarts": 0,
    "max_oom_kills": 0,
    "max_safety_faults": 0,
}

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
KV_RE = re.compile(r"([a-zA-Z0-9_]+)=([0-9]+)")
SAFETY_FAULT_TOKENS = (
    "panicked",
    "neck worker panicked",
    "neck motion stopped",
    "could not confirm neck torque off",
    "Out of memory",
    "oom-kill",
    "Main process exited, code=killed",
)


def nearest_rank_stats(values: Sequence[float]) -> Dict[str, Any]:
    """Nearest-rank percentiles matching tests/lib/stack.mjs."""
    cleaned = sorted(float(v) for v in values if isinstance(v, (int, float)) and math.isfinite(v))
    if not cleaned:
        return {"n": 0}

    def at(pct: float) -> float:
        idx = min(len(cleaned) - 1, max(0, math.ceil((pct / 100.0) * len(cleaned)) - 1))
        return round(cleaned[idx], 1)

    return {
        "n": len(cleaned),
        "p50": at(50),
        "p95": at(95),
        "p99": at(99),
        "max": round(cleaned[-1], 1),
    }


def read_wav_pcm16(path: Path) -> Tuple[int, int, List[List[int]]]:
    """Read an uncompressed 16-bit signed little-endian WAV into per-channel sample lists."""
    with wave.open(str(path), "rb") as wf:
        channels = wf.getnchannels()
        sampwidth = wf.getsampwidth()
        rate = wf.getframerate()
        nframes = wf.getnframes()
        if sampwidth != 2:
            raise ValueError(f"{path}: expected 16-bit PCM WAV, got {sampwidth * 8}-bit")
        if channels not in (1, 2):
            raise ValueError(f"{path}: expected 1 or 2 channels, got {channels}")
        if rate < 8000:
            raise ValueError(f"{path}: unsupported sample rate {rate}")
        raw = wf.readframes(nframes)
    total_samples = nframes * channels
    samples = struct.unpack(f"<{total_samples}h", raw)
    if channels == 1:
        return rate, 1, [list(samples)]
    return rate, 2, [list(samples[0::2]), list(samples[1::2])]


def frame_rms_series(samples: Sequence[int], rate: int, frame_ms: int = 10) -> List[float]:
    """Compute per-frame RMS amplitude over contiguous non-overlapping frames."""
    step = max(1, (rate * frame_ms) // 1000)
    series: List[float] = []
    for offset in range(0, len(samples) - step + 1, step):
        chunk = samples[offset : offset + step]
        mean_sq = sum(s * s for s in chunk) / float(step)
        series.append(math.sqrt(mean_sq))
    return series


def estimate_noise_floor(rms: Sequence[float], noise_frames: int) -> float:
    """Estimate background room noise RMS robustly from lead-in and lower quantile."""
    if not rms:
        return 0.0
    lead = list(rms[: max(1, min(len(rms), noise_frames))])
    lead_med = statistics.median(lead)
    ordered = sorted(rms)
    q15 = ordered[max(0, int(0.15 * (len(ordered) - 1)))]
    return min(lead_med, q15) if lead_med > 0 else q15


def find_first_sustained(
    rms: Sequence[float],
    start_idx: int,
    threshold: float,
    min_run: int,
) -> Optional[int]:
    """Return the starting frame index of the first run >= min_run above threshold."""
    run = 0
    for idx in range(max(0, start_idx), len(rms)):
        if rms[idx] >= threshold:
            run += 1
            if run >= min_run:
                return idx - min_run + 1
        else:
            run = 0
    return None


def find_prompt_segment(
    rms: Sequence[float],
    threshold: float,
    min_speech_frames: int,
    max_gap_frames: int,
) -> Optional[Tuple[int, int]]:
    """Find the first speech segment (start_idx, end_idx_exclusive) bridging short pauses."""
    start = find_first_sustained(rms, 0, threshold, min_speech_frames)
    if start is None:
        return None
    last_active = start + min_speech_frames - 1
    quiet = 0
    for idx in range(last_active + 1, len(rms)):
        if rms[idx] >= threshold:
            last_active = idx
            quiet = 0
        else:
            quiet += 1
            if quiet > max_gap_frames:
                break
    return start, last_active + 1


def find_sustained_silence(
    rms: Sequence[float],
    start_idx: int,
    silence_threshold: float,
    hold_frames: int,
) -> Optional[int]:
    """Return the first frame index where energy stays below threshold for hold_frames."""
    quiet = 0
    for idx in range(max(0, start_idx), len(rms)):
        if rms[idx] < silence_threshold:
            quiet += 1
            if quiet >= hold_frames:
                return idx - hold_frames + 1
        else:
            quiet = 0
    return None


def analyze_turn_wav(
    path: Path,
    *,
    frame_ms: int = 10,
    noise_ms: int = 300,
    min_rms: float = 300.0,
    snr_factor: float = 3.5,
    min_prompt_ms: int = 150,
    max_prompt_gap_ms: int = 250,
    min_gap_ms: int = 150,
    min_reply_ms: int = 40,
    prompt_end_ms: Optional[float] = None,
) -> Dict[str, Any]:
    """Measure end-of-speech -> first audible latency from a 16-bit PCM WAV."""
    rate, channels, ch_data = read_wav_pcm16(path)
    mic_rms = frame_rms_series(ch_data[0], rate, frame_ms)
    noise_frames = max(1, noise_ms // frame_ms)
    noise_floor = estimate_noise_floor(mic_rms, noise_frames)
    threshold = max(min_rms, noise_floor * snr_factor)

    prompt_onset_ms: Optional[float] = None
    prompt_peak_rms = 0.0
    eos_source = "mono_auto"

    if prompt_end_ms is not None:
        eos_ms = float(prompt_end_ms)
        eos_source = "explicit_prompt_end_ms"
        eos_frame = int(round(eos_ms / frame_ms))
        if eos_frame > 0:
            prompt_peak_rms = max(mic_rms[: min(len(mic_rms), eos_frame)], default=0.0)
    elif channels == 2:
        stim_rms = frame_rms_series(ch_data[1], rate, frame_ms)
        stim_noise = estimate_noise_floor(stim_rms, noise_frames)
        stim_threshold = max(200.0, stim_noise * 4.0)
        seg = find_prompt_segment(
            stim_rms,
            stim_threshold,
            max(1, min_prompt_ms // frame_ms),
            max(1, max_prompt_gap_ms // frame_ms),
        )
        if seg is None:
            raise ValueError(f"{path}: no stimulus prompt detected on channel 1")
        prompt_onset_ms = float(seg[0] * frame_ms)
        eos_ms = float(seg[1] * frame_ms)
        prompt_peak_rms = max(stim_rms[seg[0] : seg[1]], default=0.0)
        eos_source = "stereo_stimulus_ch1"
    else:
        seg = find_prompt_segment(
            mic_rms,
            threshold,
            max(1, min_prompt_ms // frame_ms),
            max(1, max_prompt_gap_ms // frame_ms),
        )
        if seg is None:
            raise ValueError(f"{path}: no prompt speech detected above RMS {threshold:.1f}")
        prompt_onset_ms = float(seg[0] * frame_ms)
        eos_ms = float(seg[1] * frame_ms)
        prompt_peak_rms = max(mic_rms[seg[0] : seg[1]], default=0.0)

    search_start = int(math.ceil((eos_ms + min_gap_ms) / frame_ms))
    reply_frame = find_first_sustained(
        mic_rms,
        search_start,
        threshold,
        max(1, min_reply_ms // frame_ms),
    )
    if reply_frame is None:
        raise ValueError(
            f"{path}: no audible reply detected after {eos_ms:.1f} ms (threshold RMS {threshold:.1f})"
        )

    first_audible_ms = float(reply_frame * frame_ms)
    reply_peak_rms = max(mic_rms[reply_frame:], default=0.0)
    latency_ms = round(first_audible_ms - eos_ms, 1)
    snr_db = (
        round(20.0 * math.log10(reply_peak_rms / max(noise_floor, 1.0)), 1)
        if reply_peak_rms > 0
        else 0.0
    )

    return {
        "file": str(path),
        "sample_rate": rate,
        "channels": channels,
        "duration_ms": round(len(ch_data[0]) * 1000.0 / rate, 1),
        "eos_source": eos_source,
        "prompt_onset_ms": prompt_onset_ms,
        "eos_ms": round(eos_ms, 1),
        "first_audible_ms": round(first_audible_ms, 1),
        "eos_to_first_audible_ms": latency_ms,
        "noise_floor_rms": round(noise_floor, 1),
        "threshold_rms": round(threshold, 1),
        "prompt_peak_rms": round(prompt_peak_rms, 1),
        "reply_peak_rms": round(reply_peak_rms, 1),
        "reply_snr_db": snr_db,
    }


def analyze_barge_wav(
    path: Path,
    *,
    frame_ms: int = 10,
    noise_ms: int = 300,
    min_rms: float = 300.0,
    snr_factor: float = 3.5,
    silence_hold_ms: int = 250,
    min_wake_ms: int = 100,
    max_wake_gap_ms: int = 200,
    wake_end_ms: Optional[float] = None,
) -> Dict[str, Any]:
    """Measure wake END -> actual playback stop latency from a 16-bit PCM WAV."""
    rate, channels, ch_data = read_wav_pcm16(path)
    mic_rms = frame_rms_series(ch_data[0], rate, frame_ms)
    noise_frames = max(1, noise_ms // frame_ms)
    noise_floor = estimate_noise_floor(mic_rms, noise_frames)
    speech_threshold = max(min_rms, noise_floor * snr_factor)
    silence_threshold = max(min_rms * 0.7, noise_floor * 2.2)

    wake_onset_ms: Optional[float] = None
    wake_source = "explicit_wake_end_ms"

    if wake_end_ms is not None:
        ref_wake_end_ms = float(wake_end_ms)
        scan_start_ms = ref_wake_end_ms
    elif channels == 2:
        stim_rms = frame_rms_series(ch_data[1], rate, frame_ms)
        stim_noise = estimate_noise_floor(stim_rms, noise_frames)
        stim_threshold = max(200.0, stim_noise * 4.0)
        seg = find_prompt_segment(
            stim_rms,
            stim_threshold,
            max(1, min_wake_ms // frame_ms),
            max(1, max_wake_gap_ms // frame_ms),
        )
        if seg is None:
            raise ValueError(f"{path}: no wake stimulus detected on channel 1")
        wake_onset_ms = float(seg[0] * frame_ms)
        ref_wake_end_ms = float(seg[1] * frame_ms)
        # In stereo mode the stimulus track is separate, so neural wake can stop
        # playback even before the wake phrase finishes.
        scan_start_ms = wake_onset_ms
        wake_source = "stereo_stimulus_ch1"
    else:
        raise ValueError(
            f"{path}: mono barge-in analysis requires --wake-end-ms (or use a stereo mic+stimulus WAV)"
        )

    pre_end = int(round((wake_onset_ms if wake_onset_ms is not None else ref_wake_end_ms) / frame_ms))
    pre_start = max(0, pre_end - max(1, 800 // frame_ms))
    active_before = sum(1 for v in mic_rms[pre_start:pre_end] if v >= speech_threshold)
    playback_was_audible = active_before >= max(1, 50 // frame_ms)
    if not playback_was_audible:
        raise ValueError(
            f"{path}: playback was not audibly active before wake ({pre_start * frame_ms}..{pre_end * frame_ms} ms)"
        )

    stop_frame = find_sustained_silence(
        mic_rms,
        int(round(scan_start_ms / frame_ms)),
        silence_threshold,
        max(1, silence_hold_ms // frame_ms),
    )
    if stop_frame is None:
        raise ValueError(
            f"{path}: playback did not reach {silence_hold_ms} ms sustained silence after wake"
        )

    playback_stop_ms = float(stop_frame * frame_ms)
    latency_ms = round(playback_stop_ms - ref_wake_end_ms, 1)
    result: Dict[str, Any] = {
        "file": str(path),
        "sample_rate": rate,
        "channels": channels,
        "duration_ms": round(len(ch_data[0]) * 1000.0 / rate, 1),
        "wake_source": wake_source,
        "wake_onset_ms": wake_onset_ms,
        "wake_end_ms": round(ref_wake_end_ms, 1),
        "playback_stop_ms": round(playback_stop_ms, 1),
        "wake_to_playback_stop_ms": latency_ms,
        "playback_was_audible": playback_was_audible,
        "noise_floor_rms": round(noise_floor, 1),
        "silence_threshold_rms": round(silence_threshold, 1),
    }
    if wake_onset_ms is not None:
        result["wake_onset_to_playback_stop_ms"] = round(playback_stop_ms - wake_onset_ms, 1)
    return result


def parse_journal_text(text: str) -> Dict[str, Any]:
    """Parse `robot-buddy` journal output for neck timing, camera activity, audio, and faults."""
    windows: List[Dict[str, int]] = []
    vision_counts: List[int] = []
    headers_ms: List[float] = []
    first_pcm_ms: List[float] = []
    camera_errors = 0
    neck_stopped = 0
    whisper_errors = 0
    safety_faults = 0
    turn_cancelled_events = 0

    for raw_line in text.splitlines():
        line = ANSI_RE.sub("", raw_line)
        if "neck loop timing" in line or "neck loop missed 20ms deadline" in line:
            kv = {m.group(1): int(m.group(2)) for m in KV_RE.finditer(line)}
            if "ticks" in kv and "missed" in kv:
                late_cycles = kv["missed"]
                skipped_slots = kv.get("skipped_slots", late_cycles)
                windows.append(
                    {
                        "period_ms": kv.get("period_ms", 20),
                        "ticks": kv["ticks"],
                        "late_cycles": late_cycles,
                        "skipped_slots": skipped_slots,
                        "total_ticks": kv.get("total_ticks", kv["ticks"]),
                        "total_late_cycles": kv.get("total_missed", late_cycles),
                        "total_skipped_slots": kv.get("total_skipped_slots", skipped_slots),
                        "p95_cycle_us": kv.get("p95_cycle_us", 0),
                        "max_cycle_us": kv.get("max_cycle_us", 0),
                        "max_work_us": kv.get("max_work_us", 0),
                        "max_overrun_us": kv.get("max_overrun_us", 0),
                    }
                )
        elif "camera inference active" in line:
            kv = {m.group(1): int(m.group(2)) for m in KV_RE.finditer(line)}
            if "vision_frames" in kv:
                vision_counts.append(kv["vision_frames"])
        elif "speech first audio" in line:
            kv = {m.group(1): int(m.group(2)) for m in KV_RE.finditer(line)}
            if "headers_ms" in kv:
                headers_ms.append(float(kv["headers_ms"]))
            if "first_pcm_ms" in kv:
                first_pcm_ms.append(float(kv["first_pcm_ms"]))

        if "kind=turn.cancelled" in line:
            turn_cancelled_events += 1
        if "camera stopped" in line or "camera unavailable" in line:
            camera_errors += 1
        if "neck motion stopped" in line or "neck worker panicked" in line:
            neck_stopped += 1
        if "local speech recognition failed" in line:
            whisper_errors += 1
        if any(token in line for token in SAFETY_FAULT_TOKENS):
            safety_faults += 1

    total_ticks = sum(w["ticks"] for w in windows)
    late_cycles = sum(w["late_cycles"] for w in windows)
    skipped_slots = sum(w["skipped_slots"] for w in windows)
    late_cycle_rate_pct = round((late_cycles * 100.0 / total_ticks), 4) if total_ticks > 0 else 0.0
    skipped_slot_rate_pct = (
        round((skipped_slots * 100.0 / total_ticks), 4) if total_ticks > 0 else 0.0
    )

    # `vision_frames` in apps/robot/src/vision/mod.rs is a monotonically cumulative
    # counter emitted at most once every 5s. Require positive progression in-window
    # (or a positive single sample on a short run) and zero camera errors.
    if not vision_counts:
        vision_frames_in_window = 0
    elif len(vision_counts) == 1:
        vision_frames_in_window = max(0, vision_counts[0])
    else:
        vision_frames_in_window = max(0, vision_counts[-1] - vision_counts[0])

    camera_active = (
        len(vision_counts) > 0 and vision_frames_in_window > 0 and camera_errors == 0
    )
    neck_active = total_ticks > 0 and neck_stopped == 0

    return {
        "motion_20ms": {
            "windows": len(windows),
            "total_ticks": total_ticks,
            "late_cycles": late_cycles,
            "skipped_slots": skipped_slots,
            "late_cycle_rate_pct": late_cycle_rate_pct,
            "skipped_slot_rate_pct": skipped_slot_rate_pct,
            "p95_cycle_us_max": max((w["p95_cycle_us"] for w in windows), default=0),
            "max_cycle_us": max((w["max_cycle_us"] for w in windows), default=0),
            "max_work_us": max((w["max_work_us"] for w in windows), default=0),
            "max_overrun_us": max((w["max_overrun_us"] for w in windows), default=0),
            "neck_active": neck_active,
            "neck_stopped_events": neck_stopped,
        },
        "vision": {
            "activity_events": len(vision_counts),
            "vision_frames_first": vision_counts[0] if vision_counts else 0,
            "vision_frames_last": vision_counts[-1] if vision_counts else 0,
            "vision_frames_in_window": vision_frames_in_window,
            "camera_active": camera_active,
            "camera_error_events": camera_errors,
        },
        "audio_journal": {
            "headers_ms": nearest_rank_stats(headers_ms),
            "first_pcm_ms": nearest_rank_stats(first_pcm_ms),
        },
        "health": {
            "camera_error_events": camera_errors,
            "neck_stopped_events": neck_stopped,
            "whisper_error_events": whisper_errors,
            "safety_fault_events": safety_faults,
            "turn_cancelled_events": turn_cancelled_events,
        },
    }


def parse_systemctl_show(output: str) -> Dict[str, Any]:
    """Parse `systemctl show <unit>` key=value output."""
    raw: Dict[str, str] = {}
    for line in output.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            raw[k.strip()] = v.strip()

    def opt_int(key: str) -> Optional[int]:
        val = raw.get(key)
        if not val or val in ("[not set]", "infinity", "18446744073709551615"):
            return None
        try:
            return int(val)
        except ValueError:
            return None

    return {
        "active_state": raw.get("ActiveState", "unknown"),
        "sub_state": raw.get("SubState", "unknown"),
        "main_pid": opt_int("MainPID"),
        "control_group": raw.get("ControlGroup") or None,
        "memory_current_bytes": opt_int("MemoryCurrent"),
        "memory_peak_bytes": opt_int("MemoryPeak"),
        "memory_max_bytes": opt_int("MemoryMax"),
        "tasks_current": opt_int("TasksCurrent"),
        "tasks_max": opt_int("TasksMax"),
        "n_restarts": opt_int("NRestarts") or 0,
        "result": raw.get("Result", "unknown"),
    }


def parse_memory_events(text: str) -> Dict[str, int]:
    """Parse cgroup v2 `memory.events` key value pairs."""
    events: Dict[str, int] = {}
    for line in text.splitlines():
        parts = line.split()
        if len(parts) == 2:
            try:
                events[parts[0]] = int(parts[1])
            except ValueError:
                pass
    return events


def read_cgroup_snapshot(
    service: str,
    *,
    systemctl_file: Optional[Path] = None,
    events_file: Optional[Path] = None,
) -> Dict[str, Any]:
    """Snapshot systemd service cgroup memory/tasks/restarts and Pi thermal state."""
    if systemctl_file is not None:
        show_text = systemctl_file.read_text(encoding="utf-8")
    else:
        props = [
            "ActiveState",
            "SubState",
            "MainPID",
            "ControlGroup",
            "MemoryCurrent",
            "MemoryPeak",
            "MemoryMax",
            "TasksCurrent",
            "TasksMax",
            "NRestarts",
            "Result",
        ]
        cmd = ["systemctl", "show", service]
        for prop in props:
            cmd.extend(["-p", prop])
        proc = subprocess.run(cmd, check=True, capture_output=True, text=True)
        show_text = proc.stdout

    info = parse_systemctl_show(show_text)

    mem_events: Dict[str, int] = {}
    if events_file is not None and events_file.is_file():
        mem_events = parse_memory_events(events_file.read_text(encoding="utf-8"))
    elif info.get("control_group"):
        cg_path = Path("/sys/fs/cgroup") / str(info["control_group"]).lstrip("/")
        ev = cg_path / "memory.events"
        if ev.is_file():
            try:
                mem_events = parse_memory_events(ev.read_text(encoding="utf-8"))
            except OSError:
                pass
        peak_file = cg_path / "memory.peak"
        if info["memory_peak_bytes"] is None and peak_file.is_file():
            try:
                info["memory_peak_bytes"] = int(peak_file.read_text(encoding="utf-8").strip())
            except (OSError, ValueError):
                pass

    throttled: Optional[str] = None
    if systemctl_file is None and shutil.which("vcgencmd"):
        try:
            res = subprocess.run(
                ["vcgencmd", "get_throttled"],
                check=False,
                capture_output=True,
                text=True,
                timeout=3,
            )
            if res.returncode == 0:
                throttled = res.stdout.strip()
        except (OSError, subprocess.SubprocessError):
            pass

    temp_c: Optional[float] = None
    thermal = Path("/sys/class/thermal/thermal_zone0/temp")
    if systemctl_file is None and thermal.is_file():
        try:
            temp_c = round(int(thermal.read_text(encoding="utf-8").strip()) / 1000.0, 1)
        except (OSError, ValueError):
            pass

    info["memory_events"] = mem_events
    info["throttled"] = throttled
    info["temp_c"] = temp_c
    return info


def evaluate_artifact(
    run: Dict[str, Any],
    *,
    targets: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """Evaluate qualification and provisional acceptance gates for one benchmark artifact."""
    t = targets or PROVISIONAL_TARGETS
    unqualified: List[str] = []
    violations: List[str] = []

    if run.get("allow_inactive") is True:
        unqualified.append("allow-inactive")
    if run.get("simulated") is True:
        unqualified.append("simulated")

    wake_mode = str(run.get("wake_mode") or "neural")
    if wake_mode not in ("neural", "transcript"):
        unqualified.append(f"unknown-wake-mode({wake_mode})")

    eos = run.get("metrics_ms", {}).get("eos_to_first_audible", {})
    barge = run.get("metrics_ms", {}).get("wake_to_playback_stop", {})
    motion = run.get("motion_20ms", {})
    vision = run.get("vision", {})
    mem = run.get("cgroup_memory", {})
    health = run.get("health", {})

    eos_n = int(eos.get("n") or 0)
    barge_n = int(barge.get("n") or 0)
    eos_p95 = eos.get("p95")
    barge_p95 = barge.get("p95")

    min_turns = int(t["min_turns"])
    min_barge = int(t["min_barge_ins_neural"])
    if eos_n < min_turns:
        unqualified.append(f"turns<{min_turns}({eos_n})")
    if eos_p95 is None or not isinstance(eos_p95, (int, float)) or not math.isfinite(eos_p95):
        unqualified.append("eos-p95-invalid")

    if wake_mode == "neural":
        if barge_n < min_barge:
            unqualified.append(f"barge_ins<{min_barge}({barge_n})")
        if (
            barge_p95 is None
            or not isinstance(barge_p95, (int, float))
            or not math.isfinite(barge_p95)
        ):
            unqualified.append("barge-p95-invalid")
    elif wake_mode == "transcript":
        # Transcript mode deliberately disables spoken barge-in to prevent speaker echo
        # self-interruption; acoustic wake-stop trials do not apply and must be N/A.
        if barge_n > 0:
            unqualified.append(f"transcript-has-barge-samples({barge_n})")

    total_ticks = int(motion.get("total_ticks") or 0)
    late_cycles = int(motion.get("late_cycles", motion.get("missed_deadlines") or 0))
    skipped_slots = int(motion.get("skipped_slots", late_cycles))
    neck_stopped = int(motion.get("neck_stopped_events", health.get("neck_stopped_events") or 0))
    neck_active = total_ticks > 0 and neck_stopped == 0
    if total_ticks <= 0:
        unqualified.append("neck-ticks=0")
    elif neck_stopped > 0:
        unqualified.append(f"neck-stopped({neck_stopped})")

    vision_events = int(vision.get("activity_events") or 0)
    vision_frames = int(vision.get("vision_frames_in_window") or 0)
    camera_errors = int(vision.get("camera_error_events", health.get("camera_error_events") or 0))
    camera_active = vision_events > 0 and vision_frames > 0 and camera_errors == 0
    if vision_events <= 0 or vision_frames <= 0:
        unqualified.append("camera-frames=0")
    if camera_errors > 0:
        unqualified.append(f"camera-errors({camera_errors})")

    # Require actual effective cgroup MemoryMax <= 1 GiB (never mask a higher cap)
    # and authoritative kernel memory.peak (250 ms polling is only a lower bound).
    target_cap = int(t["memory_limit_bytes"])
    effective_cap = mem.get("cgroup_memory_max_bytes")
    if not isinstance(effective_cap, (int, float)) or int(effective_cap) <= 0:
        unqualified.append("cgroup-limit-unknown")
        effective_cap_int = 0
    else:
        effective_cap_int = int(effective_cap)
        if effective_cap_int > target_cap:
            unqualified.append(f"cgroup-limit>1GiB({round(effective_cap_int / (1024 * 1024), 1)}MiB)")

    kernel_peak = mem.get("kernel_peak_bytes")
    if not isinstance(kernel_peak, (int, float)) or int(kernel_peak) <= 0:
        unqualified.append("cgroup-kernel-peak-unknown")
        kernel_peak_int = 0
    else:
        kernel_peak_int = int(kernel_peak)

    polled_peak = mem.get("polled_peak_lower_bound_bytes", mem.get("polled_peak_bytes"))
    polled_peak_int = (
        int(polled_peak) if isinstance(polled_peak, (int, float)) and int(polled_peak) > 0 else 0
    )

    failures = len(run.get("failures") or [])
    if failures > 0:
        unqualified.append(f"trial-failures({failures})")

    # Provisional acceptance thresholds (not measured hardware guarantees).
    eos_target = float(t["eos_to_first_audible_p95_max_ms"])
    if isinstance(eos_p95, (int, float)) and math.isfinite(eos_p95) and float(eos_p95) > eos_target:
        violations.append(f"eos_p95>{eos_target:.0f}ms")

    barge_target: Optional[float] = None
    echo_interruptions = int(
        run.get(
            "echo_interruptions",
            health.get("turn_cancelled_events", 0) if wake_mode == "transcript" else 0,
        )
    )
    if wake_mode == "neural":
        barge_target = float(t["neural_wake_end_to_playback_stop_p95_max_ms"])
        if (
            isinstance(barge_p95, (int, float))
            and math.isfinite(barge_p95)
            and float(barge_p95) > barge_target
        ):
            violations.append(f"wake_stop_p95>{barge_target:.0f}ms(neural)")
    elif wake_mode == "transcript":
        if echo_interruptions > int(t["transcript_echo_interruptions_max"]):
            violations.append(f"echo_interruptions={echo_interruptions}")

    skipped_slot_rate_pct = (
        round((skipped_slots * 100.0 / total_ticks), 4) if total_ticks > 0 else 0.0
    )
    late_cycle_rate_pct = (
        round((late_cycles * 100.0 / total_ticks), 4) if total_ticks > 0 else 0.0
    )
    max_slot_pct = float(t["max_skipped_slot_rate_pct"])
    if total_ticks > 0 and skipped_slot_rate_pct > max_slot_pct:
        violations.append(f"skipped_slots>{max_slot_pct}%")

    peak_for_budget = max(kernel_peak_int, polled_peak_int)
    budget_cap = effective_cap_int if (0 < effective_cap_int <= target_cap) else target_cap
    reserve_limit = int(budget_cap * (1.0 - float(t["memory_reserve_pct"]) / 100.0))
    within_1gib = kernel_peak_int > 0 and peak_for_budget < budget_cap and effective_cap_int <= target_cap
    within_reserve = within_1gib and peak_for_budget <= reserve_limit
    if kernel_peak_int > 0:
        if peak_for_budget >= budget_cap:
            violations.append("memory>=1GiB")
        elif peak_for_budget > reserve_limit:
            violations.append("memory>90%_reserve")

    oom_kills = int(mem.get("oom_kills") or 0)
    restarts = int(mem.get("restarts_delta") or 0)
    safety_faults = int(health.get("safety_fault_events") or 0) + neck_stopped
    if oom_kills > int(t["max_oom_kills"]):
        violations.append(f"oom_kills={oom_kills}")
    if restarts > int(t["max_restarts"]):
        violations.append(f"restarts={restarts}")
    if safety_faults > int(t["max_safety_faults"]):
        violations.append(f"safety_faults={safety_faults}")

    qualified = len(unqualified) == 0
    meets_targets = len(violations) == 0
    return {
        "qualified": qualified,
        "unqualified_reasons": unqualified,
        "meets_provisional_targets": meets_targets,
        "threshold_violations": violations,
        "passed": qualified and meets_targets,
        "wake_mode": wake_mode,
        "barge_target_ms": barge_target,
        "echo_interruptions": echo_interruptions,
        "camera_active": camera_active,
        "neck_active": neck_active,
        "vision_frames_in_window": vision_frames,
        "total_ticks": total_ticks,
        "late_cycles": late_cycles,
        "skipped_slots": skipped_slots,
        "late_cycle_rate_pct": late_cycle_rate_pct,
        "skipped_slot_rate_pct": skipped_slot_rate_pct,
        "kernel_peak_bytes": kernel_peak_int,
        "polled_peak_lower_bound_bytes": polled_peak_int,
        "effective_limit_bytes": effective_cap_int,
        "reserve_limit_bytes": reserve_limit,
        "within_1gib": within_1gib and oom_kills == 0,
        "within_reserve": within_reserve and oom_kills == 0,
        "restarts_delta": restarts,
        "oom_kills": oom_kills,
        "safety_fault_events": safety_faults,
        "failures": failures,
    }


def wav_duration_ms(path: Path) -> float:
    with wave.open(str(path), "rb") as wf:
        return wf.getnframes() * 1000.0 / wf.getframerate()


def run_on_device(args: argparse.Namespace) -> int:
    """Execute the full on-device acoustic benchmark with camera and neck active."""
    service = args.service
    barge_count = args.barge
    if barge_count is None:
        barge_count = 10 if args.wake_mode == "neural" else 0
    if args.wake_mode == "transcript" and barge_count > 0:
        print(
            "pip-perf: transcript mode deliberately disables spoken barge-in; "
            "use --barge 0 (default for transcript) or --wake-mode neural for acoustic stop trials",
            file=sys.stderr,
        )
        return 2

    limit_bytes = int(args.memory_limit_mib * 1024 * 1024)
    start_snap = read_cgroup_snapshot(service)
    if start_snap["active_state"] != "active":
        print(
            f"pip-perf: {service} is {start_snap['active_state']} (must be active)",
            file=sys.stderr,
        )
        return 2

    start_iso = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    start_epoch = time.time()

    polled_mem_bytes: List[int] = []
    polled_tasks: List[int] = []
    stop_poll = threading.Event()

    def poll_loop() -> None:
        while not stop_poll.is_set():
            try:
                snap = read_cgroup_snapshot(service)
                # Poll MemoryCurrent only so polled_peak_lower_bound_bytes stays distinct from kernel memory.peak.
                if snap["memory_current_bytes"] is not None:
                    polled_mem_bytes.append(int(snap["memory_current_bytes"]))
                if snap["tasks_current"] is not None:
                    polled_tasks.append(int(snap["tasks_current"]))
            except Exception:
                pass
            stop_poll.wait(0.25)

    poller = threading.Thread(target=poll_loop, daemon=True)
    poller.start()

    wav_dir = Path(args.save_wavs) if args.save_wavs else Path(tempfile.mkdtemp(prefix="pip-perf-"))
    wav_dir.mkdir(parents=True, exist_ok=True)
    cleanup_wavs = args.save_wavs is None

    turns_out: List[Dict[str, Any]] = []
    barges_out: List[Dict[str, Any]] = []
    failures: List[Dict[str, Any]] = []

    def record_and_play(
        out_wav: Path,
        duration_s: float,
        stim_wav: Optional[Path],
        lead_s: float,
    ) -> float:
        rec_cmd = [
            "arecord",
            "-q",
            "-D",
            args.ref_device,
            "-t",
            "wav",
            "-f",
            "S16_LE",
            "-r",
            str(args.ref_rate),
            "-c",
            str(args.ref_channels),
            "-d",
            str(int(math.ceil(duration_s))),
            str(out_wav),
        ]
        rec_start = time.monotonic()
        rec = subprocess.Popen(rec_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        stim_end_ms = 0.0
        try:
            time.sleep(lead_s)
            if stim_wav is not None and args.stim_device:
                play_start = time.monotonic()
                subprocess.run(
                    ["aplay", "-q", "-D", args.stim_device, str(stim_wav)],
                    check=True,
                    stdout=subprocess.DEVNULL,
                )
                stim_end_ms = (time.monotonic() - rec_start) * 1000.0
                expected_end = (play_start - rec_start) * 1000.0 + wav_duration_ms(stim_wav)
                stim_end_ms = min(stim_end_ms, expected_end + 80.0)
            else:
                print("pip-perf: SPEAK PROMPT NOW...", file=sys.stderr, flush=True)
            rec.wait(timeout=duration_s + 10)
        finally:
            if rec.poll() is None:
                rec.terminate()
                rec.wait(timeout=5)
        if rec.returncode != 0:
            err = rec.stderr.read().decode("utf-8", errors="replace") if rec.stderr else ""
            raise RuntimeError(f"arecord failed ({rec.returncode}): {err.strip()}")
        return stim_end_ms

    try:
        turn_wavs = [Path(p) for p in (args.turn_wav or [])]
        total_turns = args.warmup + args.turns
        for idx in range(total_turns):
            label = f"warmup-{idx + 1}" if idx < args.warmup else f"turn-{idx - args.warmup + 1}"
            wav_path = wav_dir / f"{label}.wav"
            stim = turn_wavs[idx % len(turn_wavs)] if turn_wavs else None
            print(f"pip-perf: [{args.variant}] running {label}", file=sys.stderr)
            try:
                stim_end = record_and_play(wav_path, args.turn_window_s, stim, lead_s=0.5)
                prompt_hint = (
                    stim_end
                    if (args.ref_channels == 1 and stim is not None and args.use_play_clock)
                    else None
                )
                analyzed = analyze_turn_wav(
                    wav_path,
                    min_rms=args.min_rms,
                    snr_factor=args.snr_factor,
                    prompt_end_ms=prompt_hint,
                )
                analyzed["trial"] = label
                analyzed["warmup"] = idx < args.warmup
                if idx >= args.warmup:
                    turns_out.append(analyzed)
                print(
                    f"pip-perf: {label} eos->first_audible={analyzed['eos_to_first_audible_ms']} ms "
                    f"(snr {analyzed['reply_snr_db']} dB)",
                    file=sys.stderr,
                )
            except Exception as exc:
                failures.append({"trial": label, "error": str(exc)})
                print(f"pip-perf: FAIL {label}: {exc}", file=sys.stderr)
            time.sleep(args.inter_trial_s)

        for idx in range(barge_count):
            label = f"barge-{idx + 1}"
            wav_path = wav_dir / f"{label}.wav"
            print(f"pip-perf: [{args.variant}] running {label}", file=sys.stderr)
            try:
                if args.long_wav and args.stim_device:
                    subprocess.run(
                        ["aplay", "-q", "-D", args.stim_device, args.long_wav],
                        check=True,
                        stdout=subprocess.DEVNULL,
                    )
                else:
                    print("pip-perf: SPEAK LONG-REPLY PROMPT NOW...", file=sys.stderr, flush=True)
                time.sleep(args.barge_delay_s)
                wake_path = Path(args.wake_wav) if args.wake_wav else None
                wake_end_ms = record_and_play(wav_path, args.barge_window_s, wake_path, lead_s=0.9)
                analyzed = analyze_barge_wav(
                    wav_path,
                    min_rms=args.min_rms,
                    snr_factor=args.snr_factor,
                    silence_hold_ms=args.silence_hold_ms,
                    wake_end_ms=wake_end_ms if args.ref_channels == 1 else None,
                )
                analyzed["trial"] = label
                barges_out.append(analyzed)
                print(
                    f"pip-perf: {label} wake_end->playback_stop={analyzed['wake_to_playback_stop_ms']} ms",
                    file=sys.stderr,
                )
            except Exception as exc:
                failures.append({"trial": label, "error": str(exc)})
                print(f"pip-perf: FAIL {label}: {exc}", file=sys.stderr)
            time.sleep(args.inter_trial_s)
    finally:
        stop_poll.set()
        poller.join(timeout=2)
        if cleanup_wavs:
            shutil.rmtree(wav_dir, ignore_errors=True)

    end_snap = read_cgroup_snapshot(service)
    end_iso = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")

    journal_proc = subprocess.run(
        [
            "journalctl",
            "-u",
            service,
            "--since",
            f"@{int(start_epoch)}",
            "--no-pager",
            "-o",
            "short-monotonic",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    journal_data = parse_journal_text(journal_proc.stdout)

    kernel_peak = end_snap["memory_peak_bytes"]
    polled_peak = max(polled_mem_bytes) if polled_mem_bytes else None
    restarts_delta = max(0, int(end_snap["n_restarts"]) - int(start_snap["n_restarts"]))
    oom_kills = int(end_snap["memory_events"].get("oom_kill", 0)) - int(
        start_snap["memory_events"].get("oom_kill", 0)
    )
    tasks_peak = max(
        [v for v in (polled_tasks + [end_snap["tasks_current"] or 0]) if v is not None],
        default=0,
    )

    warnings: List[str] = []
    if end_snap.get("throttled") and end_snap["throttled"] != "throttled=0x0":
        warnings.append(f"Pi reported throttling: {end_snap['throttled']}")

    artifact: Dict[str, Any] = {
        "schema_version": SCHEMA_VERSION,
        "variant": args.variant,
        "wake_mode": args.wake_mode,
        "allow_inactive": bool(args.allow_inactive),
        "simulated": False,
        "notes": args.notes or "",
        "started_at": start_iso,
        "finished_at": end_iso,
        "host": {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "throttled_start": start_snap.get("throttled"),
            "throttled_end": end_snap.get("throttled"),
            "temp_c_start": start_snap.get("temp_c"),
            "temp_c_end": end_snap.get("temp_c"),
        },
        "service": service,
        "provisional_targets": PROVISIONAL_TARGETS,
        "camera_active": journal_data["vision"]["camera_active"],
        "neck_active": journal_data["motion_20ms"]["neck_active"],
        "echo_interruptions": (
            journal_data["health"]["turn_cancelled_events"]
            if args.wake_mode == "transcript"
            else 0
        ),
        "metrics_ms": {
            "eos_to_first_audible": nearest_rank_stats(
                [t["eos_to_first_audible_ms"] for t in turns_out]
            ),
            "wake_to_playback_stop": (
                nearest_rank_stats([b["wake_to_playback_stop_ms"] for b in barges_out])
                if args.wake_mode == "neural"
                else {"n": 0, "applicable": False}
            ),
        },
        "motion_20ms": journal_data["motion_20ms"],
        "vision": journal_data["vision"],
        "audio_journal": journal_data["audio_journal"],
        "cgroup_memory": {
            "memory_limit_bytes": limit_bytes,
            "cgroup_memory_max_bytes": end_snap["memory_max_bytes"],
            "start_bytes": start_snap["memory_current_bytes"],
            "end_bytes": end_snap["memory_current_bytes"],
            "kernel_peak_bytes": kernel_peak,
            "kernel_peak_mib": (
                round(kernel_peak / (1024.0 * 1024.0), 1) if kernel_peak is not None else None
            ),
            "polled_peak_lower_bound_bytes": polled_peak,
            "polled_peak_lower_bound_mib": (
                round(polled_peak / (1024.0 * 1024.0), 1) if polled_peak is not None else None
            ),
            "limit_mib": round(limit_bytes / (1024.0 * 1024.0), 1),
            "tasks_peak": tasks_peak,
            "tasks_max": end_snap["tasks_max"],
            "restarts_delta": restarts_delta,
            "oom_kills": oom_kills,
            "memory_events_end": end_snap["memory_events"],
        },
        "health": journal_data["health"],
        "turns": turns_out,
        "barge_ins": barges_out,
        "failures": failures,
        "warnings": warnings,
    }

    verdict = evaluate_artifact(artifact)
    artifact["qualified"] = verdict["qualified"]
    artifact["unqualified_reasons"] = verdict["unqualified_reasons"]
    artifact["meets_provisional_targets"] = verdict["meets_provisional_targets"]
    artifact["threshold_violations"] = verdict["threshold_violations"]
    artifact["cgroup_memory"]["within_1gib"] = verdict["within_1gib"]
    artifact["cgroup_memory"]["within_reserve"] = verdict["within_reserve"]

    out_path = Path(args.out)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(artifact, indent=2) + "\n", encoding="utf-8")
    print(
        f"pip-perf: wrote {out_path} "
        f"(qualified={verdict['qualified']}, targets={verdict['meets_provisional_targets']})",
        file=sys.stderr,
    )
    if verdict["unqualified_reasons"]:
        print(
            f"pip-perf: UNQUALIFIED: {', '.join(verdict['unqualified_reasons'])}",
            file=sys.stderr,
        )
    if verdict["threshold_violations"]:
        print(
            f"pip-perf: PROVISIONAL TARGET VIOLATIONS: {', '.join(verdict['threshold_violations'])}",
            file=sys.stderr,
        )
    return 0 if verdict["passed"] else 1


def compare_artifacts(
    paths: Sequence[Path],
    as_json: bool = False,
) -> Tuple[str, Dict[str, Any], bool]:
    """Compare recorded Pi 5 variant JSON artifacts against baseline and provisional targets."""
    loaded = [json.loads(p.read_text(encoding="utf-8")) for p in paths]
    baseline = next((r for r in loaded if r.get("variant") == "baseline"), loaded[0])
    base_eos_p95 = baseline.get("metrics_ms", {}).get("eos_to_first_audible", {}).get("p95")
    base_barge_p95 = baseline.get("metrics_ms", {}).get("wake_to_playback_stop", {}).get("p95")

    rows: List[Dict[str, Any]] = []
    all_passed = True
    for run in loaded:
        variant = str(run.get("variant", "unknown"))
        eos = run.get("metrics_ms", {}).get("eos_to_first_audible", {})
        barge = run.get("metrics_ms", {}).get("wake_to_playback_stop", {})
        motion = run.get("motion_20ms", {})
        verdict = evaluate_artifact(run)
        if not verdict["passed"]:
            all_passed = False

        eos_p95 = eos.get("p95")
        barge_p95 = barge.get("p95")
        eos_delta = (
            round(float(eos_p95) - float(base_eos_p95), 1)
            if isinstance(eos_p95, (int, float)) and isinstance(base_eos_p95, (int, float))
            else None
        )
        barge_delta = (
            round(float(barge_p95) - float(base_barge_p95), 1)
            if isinstance(barge_p95, (int, float)) and isinstance(base_barge_p95, (int, float))
            else None
        )

        rows.append(
            {
                "variant": variant,
                "wake_mode": verdict["wake_mode"],
                "eos_n": int(eos.get("n") or 0),
                "eos_p50_ms": eos.get("p50"),
                "eos_p95_ms": eos_p95,
                "eos_p95_delta_ms": eos_delta,
                "barge_n": int(barge.get("n") or 0),
                "barge_p50_ms": barge.get("p50"),
                "barge_p95_ms": barge_p95,
                "barge_p95_delta_ms": barge_delta,
                "echo_interruptions": verdict["echo_interruptions"],
                "neck_ticks": verdict["total_ticks"],
                "neck_late_cycles": verdict["late_cycles"],
                "neck_skipped_slots": verdict["skipped_slots"],
                "neck_skipped_slot_rate_pct": verdict["skipped_slot_rate_pct"],
                "neck_max_overrun_us": int(motion.get("max_overrun_us") or 0),
                "neck_p95_cycle_us": int(motion.get("p95_cycle_us_max") or 0),
                "vision_frames_in_window": verdict["vision_frames_in_window"],
                "kernel_peak_mib": round(verdict["kernel_peak_bytes"] / (1024.0 * 1024.0), 1),
                "polled_peak_lower_bound_mib": round(
                    verdict["polled_peak_lower_bound_bytes"] / (1024.0 * 1024.0), 1
                ),
                "effective_limit_mib": round(
                    verdict["effective_limit_bytes"] / (1024.0 * 1024.0), 1
                ),
                "reserve_limit_mib": round(verdict["reserve_limit_bytes"] / (1024.0 * 1024.0), 1),
                "within_1gib": verdict["within_1gib"],
                "within_reserve": verdict["within_reserve"],
                "camera_active": verdict["camera_active"],
                "neck_active": verdict["neck_active"],
                "restarts_delta": verdict["restarts_delta"],
                "oom_kills": verdict["oom_kills"],
                "safety_fault_events": verdict["safety_fault_events"],
                "failures": verdict["failures"],
                "qualified": verdict["qualified"],
                "unqualified_reasons": verdict["unqualified_reasons"],
                "meets_provisional_targets": verdict["meets_provisional_targets"],
                "threshold_violations": verdict["threshold_violations"],
                "passed": verdict["passed"],
            }
        )

    summary = {
        "provisional_targets": PROVISIONAL_TARGETS,
        "baseline_variant": baseline.get("variant"),
        "all_passed": all_passed,
        "variants": rows,
    }
    if as_json:
        return json.dumps(summary, indent=2), summary, all_passed

    def fmt(val: Any, suffix: str = "") -> str:
        return "-" if val is None else f"{val}{suffix}"

    def fmt_delta(val: Optional[float]) -> str:
        if val is None:
            return "-"
        sign = "+" if val > 0 else ""
        return f"{sign}{val} ms"

    lines = [
        "> Provisional acceptance targets (not measured hardware guarantees): "
        "EoS → first audible p95 ≤ 3000 ms; neural wake END → stop p95 ≤ 500 ms "
        "(transcript mode: acoustic barge-in N/A, 0 echo interruptions); "
        "skipped 20 ms neck slots ≤ 0.1%; effective MemoryMax ≤ 1 GiB with kernel memory.peak "
        "< limit and 10% reserve (≤ 921.6 MiB); 0 restarts/OOM/safety faults; "
        "≥ 20 turns (and ≥ 10 barge-ins for neural) with positive camera & neck evidence.",
        "",
        "| Variant | EoS → first audible p50 / p95 (Δ vs baseline) | Wake END → stop / Echo | Neck late / skipped 20 ms slots (rate) | Camera frames | Kernel peak (polled LB) / effective cap | Status |",
        "| --- | --- | --- | --- | --- | --- | --- |",
    ]
    for r in rows:
        eos_cell = (
            f"{fmt(r['eos_p50_ms'])} / {fmt(r['eos_p95_ms'], ' ms')} "
            f"({fmt_delta(r['eos_p95_delta_ms'])}, n={r['eos_n']})"
        )
        if r["wake_mode"] == "transcript":
            barge_cell = f"N/A (transcript; echo_int={r['echo_interruptions']})"
        else:
            barge_cell = (
                f"{fmt(r['barge_p50_ms'])} / {fmt(r['barge_p95_ms'], ' ms')} "
                f"({fmt_delta(r['barge_p95_delta_ms'])}, neural, n={r['barge_n']})"
            )
        neck_cell = (
            f"{r['neck_late_cycles']} late / {r['neck_skipped_slots']} skipped "
            f"of {r['neck_ticks']} ({r['neck_skipped_slot_rate_pct']}%, max +{r['neck_max_overrun_us']} µs)"
        )
        cam_cell = f"{r['vision_frames_in_window']}"
        mem_tag = "OK" if r["within_reserve"] else ("NO-RESERVE" if r["within_1gib"] else "OVER")
        mem_cell = (
            f"{r['kernel_peak_mib']} MiB (lb {r['polled_peak_lower_bound_mib']} MiB) / "
            f"{r['effective_limit_mib']} MiB ({mem_tag})"
        )
        if r["passed"]:
            status = "PASS (meets provisional targets)"
        elif not r["qualified"]:
            status = "UNQUALIFIED (" + ", ".join(r["unqualified_reasons"]) + ")"
        else:
            status = "FAIL (" + ", ".join(r["threshold_violations"]) + ")"
        lines.append(
            f"| `{r['variant']}` | {eos_cell} | {barge_cell} | {neck_cell} | {cam_cell} | {mem_cell} | {status} |"
        )
    return "\n".join(lines), summary, all_passed


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Pip on-device acoustic performance toolkit")
    sub = parser.add_subparsers(dest="command", required=True)

    p_turn = sub.add_parser("analyze-turn", help="Analyze EoS -> first audible from a WAV")
    p_turn.add_argument("wav", type=Path)
    p_turn.add_argument("--frame-ms", type=int, default=10)
    p_turn.add_argument("--noise-ms", type=int, default=300)
    p_turn.add_argument("--min-rms", type=float, default=300.0)
    p_turn.add_argument("--snr-factor", type=float, default=3.5)
    p_turn.add_argument("--min-gap-ms", type=int, default=150)
    p_turn.add_argument("--min-reply-ms", type=int, default=40)
    p_turn.add_argument("--prompt-end-ms", type=float, default=None)

    p_barge = sub.add_parser("analyze-barge", help="Analyze wake END -> playback stop from a WAV")
    p_barge.add_argument("wav", type=Path)
    p_barge.add_argument("--frame-ms", type=int, default=10)
    p_barge.add_argument("--noise-ms", type=int, default=300)
    p_barge.add_argument("--min-rms", type=float, default=300.0)
    p_barge.add_argument("--snr-factor", type=float, default=3.5)
    p_barge.add_argument("--silence-hold-ms", type=int, default=250)
    p_barge.add_argument("--wake-end-ms", type=float, default=None)

    p_journal = sub.add_parser("parse-journal", help="Parse robot-buddy journal text")
    p_journal.add_argument("file", help="Path to journal output or '-' for stdin")

    p_cg = sub.add_parser("cgroup-snapshot", help="Read systemd cgroup memory/task metrics")
    p_cg.add_argument("--service", default="robot-buddy.service")
    p_cg.add_argument("--systemctl-file", type=Path, default=None)
    p_cg.add_argument("--events-file", type=Path, default=None)

    p_run = sub.add_parser("run", help="Run on-device acoustic benchmark against robot-buddy")
    p_run.add_argument(
        "--variant",
        required=True,
        choices=["baseline", "warm-whisper", "endpoint-shortening"],
    )
    p_run.add_argument(
        "--wake-mode",
        choices=["neural", "transcript"],
        default="neural",
        help="Wake detector active during run (neural enforces <=500ms provisional target)",
    )
    p_run.add_argument("--out", required=True, help="Output JSON artifact path")
    p_run.add_argument("--ref-device", required=True, help="ALSA capture device for reference mic")
    p_run.add_argument("--ref-rate", type=int, default=16000)
    p_run.add_argument("--ref-channels", type=int, choices=[1, 2], default=1)
    p_run.add_argument("--stim-device", default=None, help="ALSA playback device for bench speaker")
    p_run.add_argument("--turn-wav", action="append", default=[], help="Prompt WAV(s) for turns")
    p_run.add_argument("--long-wav", default=None, help="Long-reply prompt WAV for barge-in")
    p_run.add_argument("--wake-wav", default=None, help="Wake phrase WAV ('Hey Pip') for barge-in")
    p_run.add_argument("--turns", type=int, default=20)
    p_run.add_argument("--warmup", type=int, default=1)
    p_run.add_argument(
        "--barge",
        type=int,
        default=None,
        help="Barge-in trial count (defaults to 10 for neural wake, 0 for transcript mode)",
    )
    p_run.add_argument("--turn-window-s", type=float, default=12.0)
    p_run.add_argument("--barge-delay-s", type=float, default=3.5)
    p_run.add_argument("--barge-window-s", type=float, default=10.0)
    p_run.add_argument("--inter-trial-s", type=float, default=2.0)
    p_run.add_argument("--min-rms", type=float, default=300.0)
    p_run.add_argument("--snr-factor", type=float, default=3.5)
    p_run.add_argument("--silence-hold-ms", type=int, default=250)
    p_run.add_argument("--use-play-clock", action="store_true")
    p_run.add_argument("--service", default="robot-buddy.service")
    p_run.add_argument("--memory-limit-mib", type=float, default=1024.0)
    p_run.add_argument("--save-wavs", default=None)
    p_run.add_argument("--notes", default="")
    p_run.add_argument(
        "--allow-inactive",
        action="store_true",
        help="Diagnostic-only; marks the run UNQUALIFIED and rejected by compare --strict",
    )

    p_cmp = sub.add_parser(
        "compare",
        help="Compare variant JSON artifacts against provisional acceptance targets",
    )
    p_cmp.add_argument("files", nargs="+", type=Path)
    p_cmp.add_argument("--json", action="store_true", dest="as_json")
    p_cmp.add_argument(
        "--strict",
        action="store_true",
        help="Exit 1 if any artifact is unqualified, simulated/allow-inactive, or violates targets",
    )

    return parser


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    if args.command == "analyze-turn":
        result = analyze_turn_wav(
            args.wav,
            frame_ms=args.frame_ms,
            noise_ms=args.noise_ms,
            min_rms=args.min_rms,
            snr_factor=args.snr_factor,
            min_gap_ms=args.min_gap_ms,
            min_reply_ms=args.min_reply_ms,
            prompt_end_ms=args.prompt_end_ms,
        )
        print(json.dumps(result, indent=2))
        return 0
    if args.command == "analyze-barge":
        result = analyze_barge_wav(
            args.wav,
            frame_ms=args.frame_ms,
            noise_ms=args.noise_ms,
            min_rms=args.min_rms,
            snr_factor=args.snr_factor,
            silence_hold_ms=args.silence_hold_ms,
            wake_end_ms=args.wake_end_ms,
        )
        print(json.dumps(result, indent=2))
        return 0
    if args.command == "parse-journal":
        text = sys.stdin.read() if args.file == "-" else Path(args.file).read_text(encoding="utf-8")
        print(json.dumps(parse_journal_text(text), indent=2))
        return 0
    if args.command == "cgroup-snapshot":
        snap = read_cgroup_snapshot(
            args.service,
            systemctl_file=args.systemctl_file,
            events_file=args.events_file,
        )
        print(json.dumps(snap, indent=2))
        return 0
    if args.command == "run":
        return run_on_device(args)
    if args.command == "compare":
        rendered, _, ok = compare_artifacts(args.files, as_json=args.as_json)
        print(rendered)
        return 0 if (ok or not args.strict) else 1
    return 2


if __name__ == "__main__":
    sys.exit(main())
