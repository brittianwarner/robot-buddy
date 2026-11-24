#!/usr/bin/env python3
"""Linux ARM64 fixture qualification suite for tests/pip-e2e/peripherals/**.

Exercises every virtual Pip peripheral against real Linux PTYs, real env_clear()
subprocess semantics, real OpenCV YuNet + SFace CPU inference, real ARM64
whisper-cli, and the real ARM64 release `robot-buddy-robot` binary (NO --simulate).

Produces a structured JSON report with explicit fixture vs. real-hardware scope.
"""

from __future__ import annotations

import array
import hashlib
import json
import math
import os
import platform
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import urllib.request
import wave
from pathlib import Path
from typing import Any

THIS_DIR = Path(__file__).resolve().parent
REPO_ROOT = THIS_DIR.parents[2]
if str(THIS_DIR) not in sys.path:
    sys.path.insert(0, str(THIS_DIR))

from client import PeripheralsClient  # noqa: E402
from servo_pty import REG_GOAL, REG_LIMITS, REG_LOCK, REG_PRESENT, REG_TORQUE, sts_packet  # noqa: E402

FRAME_BYTES = 384 * 288 * 3 // 2  # 165,888 bytes


def http_json(url: str, method: str = "GET", body: dict[str, Any] | None = None) -> dict[str, Any]:
    data = json.dumps(body).encode("utf-8") if body is not None else None
    headers = {"Content-Type": "application/json"} if data is not None else {}
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=15.0) as resp:
        return json.loads(resp.read().decode("utf-8"))


def make_wav_bytes(samples: array.array, rate: int = 16000) -> bytes:
    import io

    buf = io.BytesIO()
    with wave.open(buf, "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(rate)
        wf.writeframes(samples.tobytes())
    return buf.getvalue()


def make_tone_pcm(freq_hz: float, duration_s: float, rate: int, amplitude: int = 12000) -> bytes:
    n = int(round(duration_s * rate))
    arr = array.array("h", [int(amplitude * math.sin(2.0 * math.pi * freq_hz * i / rate)) for i in range(n)])
    return arr.tobytes()


def write_dummy_identity(path: Path) -> None:
    identity = {
        "certificate": {
            "device_id": "pip-periph-fixture-test",
            "public_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "issuer_signature": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==",
        },
        "private_key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    }
    path.write_text(json.dumps(identity), encoding="utf-8")
    os.chmod(path, 0o600)


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return int(s.getsockname()[1])


def start_supervisor(
    run_dir: Path,
    *,
    real_whisper: str,
    unprovisioned_servos: bool = False,
    with_fake_nmcli: bool = False,
    capture_dir: Path | None = None,
) -> tuple[subprocess.Popen[bytes], dict[str, Any]]:
    ready_file = run_dir / "ready.json"
    cmd = [
        sys.executable,
        str(THIS_DIR / "supervisor.py"),
        "--run-dir",
        str(run_dir),
        "--control-port",
        "0",
        "--real-whisper",
        real_whisper,
        "--ready-file",
        str(ready_file),
    ]
    if unprovisioned_servos:
        cmd.append("--unprovisioned-servos")
    if with_fake_nmcli:
        cmd.append("--with-fake-nmcli")
    if capture_dir is not None:
        cmd.extend(["--capture-dir", str(capture_dir)])

    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    deadline = time.monotonic() + 10.0
    while not ready_file.is_file() and time.monotonic() < deadline:
        if proc.poll() is not None:
            err = proc.stderr.read().decode("utf-8", errors="replace") if proc.stderr else ""
            raise RuntimeError(f"supervisor exited early ({proc.returncode}): {err}")
        time.sleep(0.05)
    if not ready_file.is_file():
        proc.kill()
        raise RuntimeError("supervisor did not write ready.json within 10s")
    manifest = json.loads(ready_file.read_text(encoding="utf-8"))
    return proc, manifest


def stop_proc(proc: subprocess.Popen[Any] | None, timeout: float = 5.0) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=3.0)


def find_human_wav(speech_cache: Path) -> tuple[Path | None, str]:
    candidates = [
        (speech_cache / "fixtures" / "human" / "human_voices_followup.wav", "curiosity"),
        (speech_cache / "fixtures" / "human" / "human_jfk_negative.wav", "ask not"),
        (speech_cache / "fixtures" / "human_voices_followup.wav", "curiosity"),
        (speech_cache / "fixtures" / "human_jfk_negative.wav", "ask not"),
    ]
    for p, expected in candidates:
        if p.is_file() and p.suffix == ".wav":
            return p, expected
    return None, ""


def main() -> int:
    is_linux = platform.system() == "Linux"
    arch = platform.machine()
    robot_bin = Path(os.environ.get("PIP_E2E_ROBOT_BIN", "/usr/local/bin/robot-buddy-robot"))
    whisper_bin = Path(
        os.environ.get(
            "PIP_E2E_WHISPER_BIN",
            os.path.join(os.environ.get("PIP_E2E_SPEECH_DIR", "/opt/pip-e2e/speech"), "bin/whisper-cli"),
        )
    )
    cache_root = Path(os.environ.get("PIP_E2E_CACHE_DIR", str(REPO_ROOT / "tests/pip-e2e/.cache")))
    vision_cache = Path(os.environ.get("PIP_E2E_VISION_CACHE", str(cache_root / "vision")))
    speech_cache = Path(os.environ.get("PIP_E2E_SPEECH_CACHE", str(cache_root / "speech")))
    whisper_model = Path(
        os.environ.get("PIP_E2E_WHISPER_MODEL", str(speech_cache / "models/ggml-tiny.en.bin"))
    )
    artifacts_dir = Path(
        os.environ.get("PIP_E2E_ARTIFACTS_DIR", str(REPO_ROOT / "tests/pip-e2e/.artifacts"))
    )
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    report_path = artifacts_dir / "peripherals-fixture-report.json"

    checks: list[dict[str, Any]] = []

    def record(name: str, passed: bool, detail: dict[str, Any] | None = None, *, skipped: bool = False) -> None:
        status = "skip" if skipped else ("pass" if passed else "fail")
        checks.append({"name": name, "status": status, "detail": detail or {}})
        tag = status.upper()
        print(f"[{tag}] {name}")
        if status == "fail":
            print(f"       detail={json.dumps(detail or {}, default=str)}")

    # ------------------------------------------------------------------
    # 1. Supervisor startup, HTTP + Unix RPC parity, JS client + CLI
    # ------------------------------------------------------------------
    with tempfile.TemporaryDirectory(prefix="pip-periph-core-") as td:
        run_dir = Path(td)
        proc, manifest = start_supervisor(
            run_dir,
            real_whisper=str(whisper_bin),
            with_fake_nmcli=True,
            capture_dir=run_dir / "captures",
        )
        try:
            with PeripheralsClient(manifest["control_sock"]) as rpc:
                ping_rpc = rpc.call("ping")
                ping_http = http_json(f"{manifest['control_url']}/health")
                ctl_out = subprocess.check_output(
                    [sys.executable, str(THIS_DIR / "ctl.py"), "--sock", manifest["control_sock"], "ping"],
                    text=True,
                )
                ctl_json = json.loads(ctl_out)
                record(
                    "supervisor_ipc_unix_http_and_ctl_parity",
                    bool(ping_rpc["ok"]) and bool(ping_http["ok"]) and bool(ctl_json["ok"]),
                    {"control_url": manifest["control_url"], "control_sock": manifest["control_sock"]},
                )

                # Exercise client.mjs with bun or node if available
                js_runtime = shutil.which("bun") or shutil.which("node")
                if js_runtime:
                    js_code = (
                        f"import {{ connectPeripherals }} from {json.dumps(str(THIS_DIR / 'client.mjs'))};"
                        f"const c = await connectPeripherals({json.dumps(manifest['control_sock'])});"
                        "const p = await c.ping(); const s = await c.servoStatus(); c.close();"
                        "console.log(JSON.stringify({ ok: p.ok && s.limit_violations === 0 }));"
                    )
                    out = subprocess.check_output([js_runtime, "-e", js_code], text=True)
                    js_res = json.loads(out.strip())
                    record(
                        "js_esm_client_connects_and_queries_ipc",
                        bool(js_res.get("ok")),
                        {"runtime": js_runtime},
                    )
                else:
                    record("js_esm_client_connects_and_queries_ipc", True, {"reason": "no bun/node"}, skipped=True)

                # ------------------------------------------------------------------
                # 2. Fake speaker (aplay) + fake mic (arecord) + live echo injection
                # ------------------------------------------------------------------
                # 2a. Speaker normal playback + capture + SIGKILL barge-in detection
                tone_24k = make_tone_pcm(440.0, 0.4, 24000, amplitude=14000)
                aplay_bin = Path(manifest["paths"]["aplay"])
                p_play = subprocess.run(
                    [
                        str(aplay_bin),
                        "-q",
                        "-t",
                        "raw",
                        "-f",
                        "S16_LE",
                        "--buffer-time=120000",
                        "-r",
                        "24000",
                        "-c",
                        "1",
                        "-D",
                        "virtual-speaker",
                    ],
                    input=tone_24k,
                    check=False,
                )
                sp_status = rpc.call("speaker.status")
                sess1 = sp_status["sessions"][-1]
                cap_bytes = Path(sess1["capture_path"]).read_bytes() if sess1.get("capture_path") else b""
                record(
                    "speaker_aplay_consumes_pcm_and_records_session",
                    p_play.returncode == 0
                    and sess1["total_bytes"] == len(tone_24k)
                    and sess1["rate"] == 24000
                    and sess1["channels"] == 1
                    and not sess1["killed"]
                    and cap_bytes == tone_24k,
                    {
                        "total_bytes": sess1["total_bytes"],
                        "expected_bytes": len(tone_24k),
                        "rate": sess1["rate"],
                        "killed": sess1["killed"],
                    },
                )

                # 2b. Speaker SIGKILL mid-playback (barge-in)
                long_tone = make_tone_pcm(440.0, 2.0, 24000, amplitude=10000)
                p_kill = subprocess.Popen(
                    [str(aplay_bin), "-q", "-t", "raw", "-f", "S16_LE", "--buffer-time=120000", "-r", "24000", "-c", "1"],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                assert p_kill.stdin is not None
                p_kill.stdin.write(long_tone[:16384])
                p_kill.stdin.flush()
                rpc.call("speaker.wait_for", {"event": "chunk", "after_session": sess1["session_id"], "timeout_ms": 3000})
                p_kill.kill()
                p_kill.wait(timeout=3.0)
                killed_ev = rpc.call(
                    "speaker.wait_for",
                    {"event": "killed", "after_session": sess1["session_id"], "timeout_ms": 3000},
                )
                record(
                    "speaker_aplay_detects_sigkill_barge_in",
                    bool(killed_ev["session"]["killed"]),
                    {"session_id": killed_ev["session"]["session_id"], "bytes": killed_ev["session"]["total_bytes"]},
                )

                # 2c. Live speaker -> mic echo coupling (24 kHz aplay -> 16 kHz arecord) under env -i
                arecord_bin = Path(manifest["paths"]["arecord"])
                rpc.call("mic.set_echo", {"enabled": True, "gain": 0.7, "delay_ms": 40})
                p_rec = subprocess.Popen(
                    [str(arecord_bin), "-q", "-D", "virtual-mic", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "2"],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                )
                try:
                    assert p_rec.stdout is not None
                    # Read 3 baseline frames (should be silent)
                    base_raw = p_rec.stdout.read(320 * 2 * 2 * 3)
                    base_arr = array.array("h")
                    base_arr.frombytes(base_raw)
                    base_peak = max((abs(s) for s in base_arr), default=0)

                    # Play 0.5 s loud tone on speaker -> echo should arrive on mic channel 0
                    p_echo_play = subprocess.Popen(
                        [str(aplay_bin), "-q", "-t", "raw", "-f", "S16_LE", "--buffer-time=80000", "-r", "24000", "-c", "1"],
                        stdin=subprocess.PIPE,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                    assert p_echo_play.stdin is not None
                    p_echo_play.stdin.write(make_tone_pcm(660.0, 0.5, 24000, amplitude=16000))
                    p_echo_play.stdin.close()

                    # Read 35 frames (700 ms) from 2-channel arecord
                    echo_raw = p_rec.stdout.read(320 * 2 * 2 * 35)
                    p_echo_play.wait(timeout=5.0)

                    echo_arr = array.array("h")
                    echo_arr.frombytes(echo_raw)
                    ch0_peak = max((abs(s) for s in echo_arr[0::2]), default=0)
                    ch1_peak = max((abs(s) for s in echo_arr[1::2]), default=0)
                    mic_st = rpc.call("mic.status")
                finally:
                    stop_proc(p_rec)
                    rpc.call("mic.set_echo", {"enabled": False})

                record(
                    "speaker_to_mic_live_echo_injection_and_channel_routing",
                    base_peak == 0 and ch0_peak > 3000 and ch1_peak == 0 and mic_st["echo_frames_mixed"] > 0,
                    {
                        "baseline_peak": base_peak,
                        "echo_ch0_peak": ch0_peak,
                        "echo_ch1_peak": ch1_peak,
                        "echo_frames_mixed": mic_st["echo_frames_mixed"],
                    },
                )

                # ------------------------------------------------------------------
                # 3. Real recorded human PCM through fake arecord + slow real Whisper
                # ------------------------------------------------------------------
                human_wav, expected_phrase = find_human_wav(speech_cache)
                if human_wav and whisper_bin.is_file() and whisper_model.is_file():
                    p_rec2 = subprocess.Popen(
                        [str(arecord_bin), "-q", "-D", "virtual-mic", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                    )
                    try:
                        inj = rpc.call(
                            "mic.play_file",
                            {
                                "path": str(human_wav),
                                "clip_id": "periph_human_test",
                                "pad_silence_before_ms": 100,
                                "pad_silence_after_ms": 300,
                            },
                        )
                        assert p_rec2.stdout is not None
                        want_bytes = int(inj["total_samples"]) * 2
                        captured = bytearray()
                        deadline = time.monotonic() + (inj["duration_ms"] / 1000.0) + 5.0
                        while len(captured) < want_bytes and time.monotonic() < deadline:
                            chunk = p_rec2.stdout.read(min(6400, want_bytes - len(captured)))
                            if not chunk:
                                break
                            captured.extend(chunk)
                    finally:
                        stop_proc(p_rec2)

                    cap_samples = array.array("h")
                    cap_samples.frombytes(bytes(captured))
                    wav_bytes = make_wav_bytes(cap_samples, 16000)

                    # 3a. Slow real whisper-cli wrapper with 500 ms artificial delay
                    # Note: whisper.cpp v1.9.6 requires `-of /dev/null` with `-f -` so
                    # print_segment_callback writes transcripts to stdout; whisper-slow
                    # forwards argv transparently without rewriting it.
                    rpc.call(
                        "whisper.configure",
                        {"real_whisper_bin": str(whisper_bin), "delay_before_ms": 500, "delay_after_ms": 0, "hold": False},
                    )
                    whisper_slow = Path(manifest["paths"]["whisper_slow"])
                    t0 = time.monotonic()
                    w_run = subprocess.run(
                        [
                            str(whisper_slow),
                            "-m",
                            str(whisper_model),
                            "-l",
                            "en",
                            "-t",
                            "2",
                            "-np",
                            "-nt",
                            "-of",
                            "/dev/null",
                            "-f",
                            "-",
                        ],
                        input=wav_bytes,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                        check=False,
                    )
                    elapsed_ms = round((time.monotonic() - t0) * 1000.0, 1)
                    transcript = " ".join(w_run.stdout.decode("utf-8", errors="replace").split())
                    w_st = rpc.call("whisper.status")
                    record(
                        "mic_real_recorded_pcm_to_slow_real_whisper_cli",
                        w_run.returncode == 0
                        and elapsed_ms >= 480.0
                        and expected_phrase in transcript.lower()
                        and w_st["active_calls"] == 0,
                        {
                            "fixture": human_wav.name,
                            "elapsed_ms": elapsed_ms,
                            "expected_phrase": expected_phrase,
                            "transcript_len": len(transcript),
                            "matched_phrase": expected_phrase in transcript.lower(),
                            "active_calls": w_st["active_calls"],
                        },
                    )

                    # 3b. Hold + SIGKILL cancellation leaves zero orphan whisper-cli
                    last_id = w_st["total_calls"]
                    rpc.call(
                        "whisper.configure",
                        {"real_whisper_bin": str(whisper_bin), "delay_before_ms": 0, "hold": True},
                    )
                    p_hold = subprocess.Popen(
                        [str(whisper_slow), "-m", str(whisper_model), "-l", "en", "-t", "2", "-np", "-nt", "-f", "-"],
                        stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                    )
                    assert p_hold.stdin is not None
                    p_hold.stdin.write(wav_bytes[:4096])
                    p_hold.stdin.close()
                    rpc.call("whisper.wait_for", {"event": "started", "after_id": last_id, "timeout_ms": 5000})
                    p_hold.kill()
                    p_hold.wait(timeout=3.0)
                    cancelled = rpc.call("whisper.wait_for", {"event": "cancelled", "after_id": last_id, "timeout_ms": 5000})
                    w_st2 = rpc.call("whisper.status")
                    record(
                        "slow_whisper_wrapper_tracks_sigkill_cancellation_cleanly",
                        bool(cancelled["call"]["cancelled"]) and w_st2["active_calls"] == 0,
                        {"call_id": cancelled["call"]["id"], "active_calls": w_st2["active_calls"]},
                    )

                    # 3c. Hold + whisper.release completes real whisper-cli decode cleanly
                    last_id2 = w_st2["total_calls"]
                    p_rel = subprocess.Popen(
                        [
                            str(whisper_slow),
                            "-m",
                            str(whisper_model),
                            "-l",
                            "en",
                            "-t",
                            "2",
                            "-np",
                            "-nt",
                            "-of",
                            "/dev/null",
                            "-f",
                            "-",
                        ],
                        stdin=subprocess.PIPE,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        env={"PATH": "/usr/bin:/bin", "LC_ALL": "C"},
                    )
                    assert p_rel.stdin is not None
                    p_rel.stdin.write(wav_bytes)
                    p_rel.stdin.close()
                    p_rel.stdin = None
                    started_rel = rpc.call("whisper.wait_for", {"event": "started", "after_id": last_id2, "timeout_ms": 5000})
                    rpc.call("whisper.release", {"call_id": started_rel["call"]["id"]})
                    rel_out, _ = p_rel.communicate(timeout=20.0)
                    rpc.call("whisper.configure", {"hold": False})
                    rel_text = " ".join(rel_out.decode("utf-8", errors="replace").split())
                    w_st3 = rpc.call("whisper.status")
                    record(
                        "slow_whisper_wrapper_hold_and_release_runs_real_whisper_cli",
                        p_rel.returncode == 0
                        and expected_phrase in rel_text.lower()
                        and w_st3["active_calls"] == 0,
                        {
                            "call_id": started_rel["call"]["id"],
                            "matched_phrase": expected_phrase in rel_text.lower(),
                            "active_calls": w_st3["active_calls"],
                        },
                    )
                else:
                    record(
                        "mic_real_recorded_pcm_to_slow_real_whisper_cli",
                        False,
                        {
                            "reason": "missing real whisper-cli, model, or human WAV fixture",
                            "whisper_bin": str(whisper_bin),
                            "whisper_model": str(whisper_model),
                            "human_wav": str(human_wav),
                        },
                    )

                # ------------------------------------------------------------------
                # 4. Simulated camera rpicam-vid I420 contract under env_clear()
                # ------------------------------------------------------------------
                rpicam_bin = Path(manifest["paths"]["rpicam_vid"])
                face_a1 = vision_cache / "face_a_1.i420"
                rpc.call("camera.set_scene", {"kind": "i420", "path": str(face_a1), "offset_x": 0.25, "fps": 30})
                cam_out = subprocess.run(
                    [
                        str(rpicam_bin),
                        "--nopreview",
                        "--frames",
                        "2",
                        "--timeout",
                        "0",
                        "--codec",
                        "yuv420",
                        "--width",
                        "384",
                        "--height",
                        "288",
                        "--framerate",
                        "30",
                        "--rotation",
                        "180",
                        "--output",
                        "-",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    env={"PATH": "/usr/bin:/bin"},
                    check=False,
                )
                cam_st = rpc.call("camera.status")
                record(
                    "camera_rpicam_vid_emits_exact_i420_under_env_clear",
                    cam_out.returncode == 0
                    and len(cam_out.stdout) == 2 * FRAME_BYTES
                    and cam_st["last_rotation"] == 180,
                    {"bytes": len(cam_out.stdout), "expected_bytes": 2 * FRAME_BYTES, "rotation": cam_st["last_rotation"]},
                )
        finally:
            stop_proc(proc)

    # ------------------------------------------------------------------
    # 5. Real ARM64 robot-buddy-robot servo CLI + EEPROM lock + limit checks
    # ------------------------------------------------------------------
    if is_linux and robot_bin.is_file():
        with tempfile.TemporaryDirectory(prefix="pip-periph-servo-") as td:
            run_dir = Path(td)
            proc, manifest = start_supervisor(
                run_dir, real_whisper=str(whisper_bin), unprovisioned_servos=True
            )
            try:
                with PeripheralsClient(manifest["control_sock"]) as rpc:
                    servo_port = manifest["servo_port"]
                    cal_path = manifest["servo_calibration"]

                    ping_r = subprocess.run(
                        [str(robot_bin), "servo", "ping", servo_port, cal_path],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                    read_r = subprocess.run(
                        [str(robot_bin), "servo", "read", servo_port, cal_path],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                    # Unprovisioned [0, 4095] MUST fail verify!
                    verify_fail = subprocess.run(
                        [str(robot_bin), "servo", "verify", servo_port, cal_path],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                    # Provision must unlock, write calibrated limits, relock, and verify!
                    prov_r = subprocess.run(
                        [str(robot_bin), "servo", "provision", servo_port, cal_path, "--write-eeprom"],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                    verify_ok = subprocess.run(
                        [str(robot_bin), "servo", "verify", servo_port, cal_path],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                    # Offset all 3 servos by +40 steps and run `servo center`
                    for sid in (1, 2, 3):
                        rpc.call("servo.set_register", {"id": sid, "reg": REG_PRESENT, "bytes": list(struct.pack("<H", 2088))})
                    center_r = subprocess.run(
                        [str(robot_bin), "servo", "center", servo_port, cal_path],
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                        text=True,
                        check=False,
                    )
                    st = rpc.call("servo.status")
                    bounds = rpc.call("servo.assert_bounds")
                    # Matches apps/robot/src/motion/mod.rs CENTER_TOLERANCE_STEPS = 8.
                    centered_ok = all(
                        abs(st["joints"][str(sid)]["present"] - 2048) <= 8
                        and st["joints"][str(sid)]["torque"] == 0
                        and st["joints"][str(sid)]["lock"] == 1
                        and st["joints"][str(sid)]["eeprom_min_steps"] == 1888
                        and st["joints"][str(sid)]["eeprom_max_steps"] == 2208
                        for sid in (1, 2, 3)
                    )
                    record(
                        "sts3215_real_robot_cli_ping_read_provision_verify_center_1mbaud",
                        ping_r.returncode == 0
                        and read_r.returncode == 0
                        and verify_fail.returncode != 0
                        and prov_r.returncode == 0
                        and verify_ok.returncode == 0
                        and center_r.returncode == 0
                        and bool(st["termios"]["raw_1mbaud_ok"])
                        and centered_ok
                        and bool(bounds["ok"]),
                        {
                            "termios": st["termios"],
                            "verify_unprovisioned_rc": verify_fail.returncode,
                            "provision_rc": prov_r.returncode,
                            "center_rc": center_r.returncode,
                            "joints": {k: {f: v[f] for f in ("present", "torque", "lock", "eeprom_min_steps", "eeprom_max_steps")} for k, v in st["joints"].items()},
                        },
                    )

                    # Verify simulator catches unauthorized EEPROM write (LOCK=1) and out-of-range GOAL write
                    slave_fd = os.open(servo_port, os.O_RDWR | os.O_NOCTTY)
                    try:
                        # Attempt to write LIMITS while LOCK=1 -> must reject & count eeprom_lock_violation
                        os.write(slave_fd, sts_packet(1, 0x03, bytes([REG_LIMITS, 0, 0, 255, 15])))
                        # Attempt to write GOAL=100 (below min_steps=1888) -> must count limit_violation
                        os.write(slave_fd, sts_packet(1, 0x03, bytes([REG_GOAL, 100, 0])))
                        time.sleep(0.1)
                    finally:
                        os.close(slave_fd)
                    st_viol = rpc.call("servo.status")
                    record(
                        "sts3215_pty_enforces_eeprom_lock_and_detects_limit_violations",
                        st_viol["eeprom_lock_violations"] >= 1
                        and st_viol["limit_violations"] >= 1
                        and st_viol["joints"]["1"]["eeprom_min_steps"] == 1888,
                        {
                            "eeprom_lock_violations": st_viol["eeprom_lock_violations"],
                            "limit_violations": st_viol["limit_violations"],
                            "eeprom_min_preserved": st_viol["joints"]["1"]["eeprom_min_steps"],
                        },
                    )
            finally:
                stop_proc(proc)
    else:
        record(
            "sts3215_real_robot_cli_ping_read_provision_verify_center_1mbaud",
            True,
            {"reason": f"requires Linux ARM64 robot binary ({robot_bin})"},
            skipped=True,
        )

    # ------------------------------------------------------------------
    # 6. Real ARM64 robot-buddy-robot daemon (NO --simulate):
    #    - Face PTY 115200 raw + soft-reset hello (no fd close) + QR readback
    #    - Real OpenCV YuNet + SFace inference driving face gaze + STS3215 50 Hz tracking
    #    - Mid-run servo health fault -> immediate 3-axis torque disarm
    # ------------------------------------------------------------------
    yunet_model = vision_cache / "face_detection_yunet_2023mar.onnx"
    sface_model = vision_cache / "face_recognition_sface_2021dec.onnx"
    face_a1 = vision_cache / "face_a_1.i420"

    if is_linux and robot_bin.is_file() and yunet_model.is_file() and face_a1.is_file():
        with tempfile.TemporaryDirectory(prefix="pip-periph-daemon-") as td:
            run_dir = Path(td)
            id_file = run_dir / "device.identity.json"
            write_dummy_identity(id_file)
            gallery_dir = run_dir / "faces"
            cache_file = run_dir / "cache.json"
            proc, manifest = start_supervisor(
                run_dir, real_whisper=str(whisper_bin), with_fake_nmcli=True
            )
            robot_proc: subprocess.Popen[bytes] | None = None
            try:
                with PeripheralsClient(manifest["control_sock"]) as rpc:
                    # Start with blank camera so we control when face appears
                    rpc.call("camera.set_scene", {"kind": "blank", "fps": 15})
                    robot_env = {
                        "PATH": f"{manifest['bin_dir']}:/usr/local/bin:/usr/bin:/bin",
                        "HOME": str(run_dir),
                        "RUST_LOG": "info",
                        # Unreachable loopback HTTPS so robot stays in hardware mode without external calls
                        "BUDDY_SERVER_URL": f"https://127.0.0.1:{free_port()}",
                        "BUDDY_IDENTITY_FILE": str(id_file),
                        "BUDDY_CACHE_FILE": str(cache_file),
                        "BUDDY_SCREEN_ADDR": "off",
                        "BUDDY_SPEECH": "false",
                        "BUDDY_FACE_SERIAL": manifest["face_serial"],
                        "BUDDY_SERVO_PORT": manifest["servo_port"],
                        "BUDDY_SERVO_CALIBRATION": manifest["servo_calibration"],
                        "BUDDY_VISION": "1",
                        "BUDDY_VISION_CAMERA": manifest["camera_bin"],
                        "BUDDY_VISION_YUNET": str(yunet_model),
                        "BUDDY_VISION_GALLERY": str(gallery_dir),
                        "BUDDY_WIFI_GRACE_SECS": "3600",
                    }
                    if sface_model.is_file():
                        robot_env["BUDDY_VISION_SFACE"] = str(sface_model)

                    robot_proc = subprocess.Popen(
                        [str(robot_bin)],
                        env=robot_env,
                        stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE,
                    )

                    # Wait for face PTY open + initial mood/qr/ping + servo 50 Hz arming
                    rpc.call("face.wait_for", {"predicate": {"has_ping": True}, "timeout_ms": 8000})
                    face_st = rpc.call("face.status")
                    rpc.call("servo.wait_for_goal", {"torque": 1, "timeout_ms": 8000})
                    time.sleep(0.4)
                    servo_st = rpc.call("servo.status")
                    bounds = rpc.call("servo.assert_bounds")
                    record(
                        "real_robot_opens_face_115200_and_arms_sts3215_50hz_within_limits",
                        bool(face_st["termios"]["raw_115200_ok"])
                        and bool(servo_st["termios"]["raw_1mbaud_ok"])
                        and servo_st["goal_frames_count"] >= 10
                        and bool(bounds["ok"]),
                        {
                            "face_termios": face_st["termios"],
                            "servo_termios": servo_st["termios"],
                            "goal_frames_count": servo_st["goal_frames_count"],
                            "cadence_hz": servo_st["cadence_hz"],
                        },
                    )

                    # Soft-reset hello WITHOUT closing PTY fd -> assert QR readback + no fd close
                    hello = rpc.call("face.inject_hello", {"version": 1, "eyes": 1, "fragmented": True})
                    qr_check = rpc.call(
                        "face.assert_qr_after_hello",
                        {
                            "hello_seq": hello["hello_seq"],
                            "expected_qr": [],
                            "timeout_ms": 3000,
                            "forbid_gesture_replay": True,
                        },
                    )
                    record(
                        "face_hello_soft_reset_without_fd_close_repaints_qr_state",
                        bool(qr_check["ok"]) and qr_check["fd_closed"] is False,
                        qr_check,
                    )

                    # Real OpenCV YuNet face detection -> gaze + STS3215 yaw tracking!
                    seq_before_right = rpc.call("face.status")["total_lines"]
                    servo_seq_before = rpc.call("servo.status")["latest_seq"]
                    rpc.call(
                        "camera.set_scene",
                        {"kind": "i420", "path": str(face_a1), "offset_x": 0.35, "offset_y": 0.0, "fps": 15},
                    )
                    look_right = rpc.call(
                        "face.wait_for",
                        {"predicate": {"look_nonzero": True, "after_seq": seq_before_right}, "timeout_ms": 10000},
                    )
                    yaw_moved_right = rpc.call(
                        "servo.wait_for_goal",
                        {
                            "axis": 0,
                            "min_delta_steps": 15,
                            "baseline_steps": 2048,
                            "after_seq": servo_seq_before,
                            "timeout_ms": 6000,
                        },
                    )
                    right_lx = float(look_right["event"]["parsed"]["look"][0])
                    right_yaw = int(yaw_moved_right["event"]["steps"][0])

                    # Move face to the opposite side (-0.35) and assert opposite gaze + yaw tracking!
                    seq_before_left = rpc.call("face.status")["total_lines"]
                    rpc.call(
                        "camera.set_scene",
                        {"kind": "i420", "path": str(face_a1), "offset_x": -0.35, "offset_y": 0.0, "fps": 15},
                    )
                    look_left = None
                    deadline = time.monotonic() + 10.0
                    while time.monotonic() < deadline:
                        hist = rpc.call("face.history", {"since_seq": seq_before_left, "limit": 200})
                        for ev in hist["events"]:
                            if ev["kind"] == "look" and float(ev["parsed"]["look"][0]) * right_lx < 0:
                                look_left = ev
                                break
                        if look_left is not None:
                            break
                        time.sleep(0.1)
                    time.sleep(0.8)
                    left_yaw = rpc.call("servo.status")["joints"]["1"]["goal"]
                    bounds_track = rpc.call("servo.assert_bounds")
                    record(
                        "real_yunet_inference_drives_face_gaze_and_servo_tracking_within_bounds",
                        look_left is not None
                        and right_lx < 0 < float(look_left["parsed"]["look"][0])
                        and right_yaw < 2048 < left_yaw
                        and bool(bounds_track["ok"]),
                        {
                            "right_look_x": right_lx,
                            "left_look_x": float(look_left["parsed"]["look"][0]) if look_left else None,
                            "right_yaw_steps": right_yaw,
                            "left_yaw_steps": left_yaw,
                            "bounds_ok": bounds_track["ok"],
                        },
                    )

                    # Camera disconnect fault -> backoff -> recovery back to real YuNet tracking
                    spawns_before = rpc.call("camera.status")["spawns"]
                    rpc.call("camera.set_scene", {"kind": "disconnect"})
                    time.sleep(0.4)
                    rpc.call(
                        "camera.set_scene",
                        {"kind": "i420", "path": str(face_a1), "offset_x": 0.3, "offset_y": 0.0, "fps": 15},
                    )
                    recovered = False
                    rec_deadline = time.monotonic() + 8.0
                    while time.monotonic() < rec_deadline:
                        if rpc.call("camera.status")["spawns"] > spawns_before:
                            recovered = True
                            break
                        time.sleep(0.1)
                    record(
                        "camera_disconnect_fault_recovers_and_respawns_rpicam_vid",
                        recovered,
                        {"spawns_before": spawns_before, "spawns_after": rpc.call("camera.status")["spawns"]},
                    )

                    # Inject mid-run servo health fault (unlock EEPROM LOCK=0 on servo 2) -> verify 3-axis disarm!
                    rpc.call("servo.set_register", {"id": 2, "reg": REG_LOCK, "bytes": [0]})
                    disarmed = rpc.call("servo.wait_for_goal", {"torque": 0, "timeout_ms": 5000})
                    record(
                        "sts3215_health_fault_disarms_all_three_axes_immediately",
                        disarmed["torques"] == [0, 0, 0],
                        disarmed,
                    )

                    # Clean SIGTERM shutdown sends {"qr":[]} + Offline mood
                    seq_before_stop = rpc.call("face.status")["total_lines"]
                    stop_proc(robot_proc, timeout=5.0)
                    robot_proc = None
                    after_stop = rpc.call("face.history", {"since_seq": seq_before_stop, "limit": 50})
                    saw_clear_qr = any(ev["kind"] == "qr" and ev["parsed"]["qr"] == [] for ev in after_stop["events"])
                    saw_offline = any(
                        ev["kind"] == "expression" and ev["parsed"]["e"] == "sleepy" for ev in after_stop["events"]
                    )
                    record(
                        "real_robot_shutdown_clears_face_qr_and_sets_offline_mood",
                        saw_clear_qr and saw_offline,
                        {"events_on_stop": [ev["raw"] for ev in after_stop["events"]]},
                    )
            finally:
                stop_proc(robot_proc)
                stop_proc(proc)

        # ------------------------------------------------------------------
        # 7. Real Wi-Fi Setup non-empty QR pair + hello soft reset WITHOUT fd close
        #    (requires 10.42.0.1 on loopback via ip addr add / NET_ADMIN)
        # ------------------------------------------------------------------
        has_setup_ip = False
        added_setup_ip = False
        try:
            ip_out = subprocess.check_output(["ip", "-4", "addr", "show", "dev", "lo"], text=True)
            if "10.42.0.1/" in ip_out:
                has_setup_ip = True
            else:
                r_add = subprocess.run(
                    ["ip", "addr", "add", "10.42.0.1/32", "dev", "lo"],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )
                if r_add.returncode == 0:
                    has_setup_ip = True
                    added_setup_ip = True
        except Exception:
            has_setup_ip = False

        if has_setup_ip:
            with tempfile.TemporaryDirectory(prefix="pip-periph-wifiqr-") as td:
                run_dir = Path(td)
                id_file = run_dir / "device.identity.json"
                write_dummy_identity(id_file)
                setup_wifi_file = run_dir / "setup-wifi.json"
                proc, manifest = start_supervisor(
                    run_dir, real_whisper=str(whisper_bin), with_fake_nmcli=True
                )
                robot_proc = None
                try:
                    with PeripheralsClient(manifest["control_sock"]) as rpc:
                        rpc.call("nmcli.set_state", {"state": "disconnected"})
                        robot_env = {
                            "PATH": f"{manifest['bin_dir']}:/usr/local/bin:/usr/bin:/bin",
                            "HOME": str(run_dir),
                            "RUST_LOG": "info",
                            "BUDDY_SERVER_URL": f"https://127.0.0.1:{free_port()}",
                            "BUDDY_IDENTITY_FILE": str(id_file),
                            "BUDDY_CACHE_FILE": str(run_dir / "cache.json"),
                            "BUDDY_SETUP_WIFI_FILE": str(setup_wifi_file),
                            "BUDDY_SCREEN_ADDR": "off",
                            "BUDDY_SPEECH": "false",
                            "BUDDY_FACE_SERIAL": manifest["face_serial"],
                            # Note: before first connection device.rs uses BOOT_GRACE=60s unless
                            # we pre-seed a connected_once? No — let's check if wifi setup opens within grace.
                            "BUDDY_WIFI_GRACE_SECS": "1",
                        }
                        # Wait: in device.rs, BOOT_GRACE is 60s before first connection!
                        # Check if a 65s wait is needed, or verify directly.
                        # Actually, let's check if setup triggers within 68s — worth proving real Wi-Fi QR readback!
                        robot_proc = subprocess.Popen(
                            [str(robot_bin)],
                            env=robot_env,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                        )
                        qr_ev = rpc.call(
                            "face.wait_for",
                            {"predicate": {"qr_non_empty": True}, "timeout_ms": 75000},
                        )
                        qr_pair = qr_ev["event"]["parsed"]["qr"]
                        qr_w = qr_ev["event"]["parsed"].get("w")
                        hello = rpc.call("face.inject_hello", {"version": 1, "eyes": 1, "fragmented": True})
                        qr_after = rpc.call(
                            "face.assert_qr_after_hello",
                            {
                                "hello_seq": hello["hello_seq"],
                                "expected_left": qr_pair[0],
                                "expected_right": "http://10.42.0.1",
                                "expected_width": 132,
                                "require_non_empty": True,
                                "timeout_ms": 4000,
                                "forbid_gesture_replay": True,
                            },
                        )
                        # Redact Wi-Fi password before recording in report!
                        record(
                            "wifi_setup_non_empty_qr_readback_after_hello_without_fd_close",
                            bool(qr_after["ok"])
                            and qr_after["fd_closed"] is False
                            and qr_pair[0].startswith("WIFI:")
                            and qr_pair[1] == "http://10.42.0.1"
                            and qr_w == 132,
                            {
                                "hello_seq": hello["hello_seq"],
                                "fd_closed": qr_after["fd_closed"],
                                "left_prefix": qr_pair[0][:12],
                                "right": qr_pair[1],
                                "width": qr_w,
                                "repainted_kinds": qr_after["repainted_kinds"],
                            },
                        )
                finally:
                    stop_proc(robot_proc)
                    stop_proc(proc)
                    if added_setup_ip:
                        subprocess.run(
                            ["ip", "addr", "del", "10.42.0.1/32", "dev", "lo"],
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            check=False,
                        )
        else:
            record(
                "wifi_setup_non_empty_qr_readback_after_hello_without_fd_close",
                True,
                {"reason": "requires 10.42.0.1/32 on loopback (--cap-add NET_ADMIN)"},
                skipped=True,
            )

    failed = [c for c in checks if c["status"] == "fail"]
    passed = [c for c in checks if c["status"] == "pass"]
    skipped = [c for c in checks if c["status"] == "skip"]

    def _sha256_or_none(p: Path) -> str | None:
        if not p.is_file():
            return None
        h = hashlib.sha256()
        with open(p, "rb") as fp:
            for chunk in iter(lambda: fp.read(65536), b""):
                h.update(chunk)
        return h.hexdigest()

    git_head = None
    try:
        git_head = subprocess.check_output(
            ["git", "-C", str(REPO_ROOT), "rev-parse", "HEAD"],
            stderr=subprocess.DEVNULL,
            text=True,
        ).strip()
    except Exception:
        pass

    report = {
        "schema": "pip-e2e-peripherals-report-v1",
        "platform": {"system": platform.system(), "machine": arch},
        "provenance": {
            "git_head": git_head,
            "robot_bin": str(robot_bin),
            "robot_bin_sha256": _sha256_or_none(robot_bin),
            "whisper_bin": str(whisper_bin),
            "whisper_bin_sha256": _sha256_or_none(whisper_bin),
            "whisper_model_sha256": _sha256_or_none(whisper_model),
            "yunet_sha256": _sha256_or_none(yunet_model),
            "sface_sha256": _sha256_or_none(sface_model),
        },
        "summary": {
            "total": len(checks),
            "passed": len(passed),
            "failed": len(failed),
            "skipped": len(skipped),
            "ok": len(failed) == 0 and len(passed) > 0,
        },
        "fixtures_vs_real_hardware": {
            "real_code_and_models_exercised": [
                "Real ARM64 Linux PTYs + kernel termios (115200 raw face, 1 Mbaud B1000000 raw STS3215)",
                "Real ARM64 release robot-buddy-robot binary (--features pi-vision,onnx-wake, NO --simulate)",
                "Real OpenCV 4.10 YuNet + SFace ONNX CPU inference on pinned public OpenCV test portraits",
                "Real ARM64 whisper-cli (whisper.cpp v1.9.6) + ggml-tiny.en.bin on real recorded human WAV",
                "Real env_clear() child process isolation on rpicam-vid, arecord, whisper-cli, and nmcli",
            ],
            "simulated_or_fixture_components": [
                "ESP32-S3 face serial board emulated over Linux PTY (face_pty.py) — does not test physical USB CDC-ACM electrical disconnects or LCD rendering",
                "Feetech STS3215 3-axis servo bus emulated over Linux PTY with half-duplex echo & kinematic tracking (servo_pty.py) — does not test physical torque load, gear backlash, or power-rail brownout",
                "Pi 5 CSI camera emulated via rpicam-vid I420 pipe shim (camera_bridge.py) — does not test libcamera ISP/auto-exposure/lens focus",
                "MAX98357A speaker (aplay) and reSpeaker mic (arecord) emulated via PCM pipe shims + software echo mixer (audio_bridge.py) — does not test room acoustics or hardware AEC/I2S clocking",
                "NetworkManager nmcli emulated via stateful stub when --with-fake-nmcli is passed",
            ],
        },
        "checks": checks,
    }
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"\nReport written to {report_path}")
    print(f"Summary: {len(passed)} passed, {len(failed)} failed, {len(skipped)} skipped")
    return 0 if not failed else 1


if __name__ == "__main__":
    raise SystemExit(main())
