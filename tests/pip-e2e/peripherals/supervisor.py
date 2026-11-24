#!/usr/bin/env python3
"""Master ARM64 virtual Pip peripheral supervisor.

Owns and coordinates:
- Face serial PTY (<run_dir>/dev/face)
- STS3215 3-axis neck serial PTY (<run_dir>/dev/servo + <run_dir>/neck.json)
- Simulated rpicam-vid I420 camera (<run_dir>/bin/rpicam-vid + <run_dir>/ipc/camera.sock)
- Fake ALSA speaker (<run_dir>/bin/aplay + <run_dir>/ipc/speaker.sock)
- Fake ALSA mic (<run_dir>/bin/arecord + <run_dir>/ipc/mic.sock) + live echo coupling
- Slow real Whisper wrapper (<run_dir>/bin/whisper-slow + <run_dir>/ipc/whisper.sock)
- Optional fake nmcli (<run_dir>/bin/nmcli when --with-fake-nmcli is passed)

Exposes BOTH:
- Unix domain socket newline-delimited JSON-RPC at <run_dir>/control.sock
- Loopback HTTP control API at http://127.0.0.1:<port>
"""

from __future__ import annotations

import argparse
import http.server
import json
import os
import shlex
import signal
import socket
import sys
import threading
import time
import urllib.parse
from pathlib import Path
from typing import Any

THIS_DIR = Path(__file__).resolve().parent
if str(THIS_DIR) not in sys.path:
    sys.path.insert(0, str(THIS_DIR))

from audio_bridge import AudioBridge  # noqa: E402
from camera_bridge import CameraBridge  # noqa: E402
from face_pty import FacePty  # noqa: E402
from servo_pty import ServoPty  # noqa: E402


def _write_launcher(dest: Path, script: Path, flag_name: str, flag_val: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    body = (
        "#!/bin/sh\n"
        f"exec {shlex.quote(sys.executable)} {shlex.quote(str(script))} "
        f"{flag_name} {shlex.quote(str(flag_val))} -- \"$@\"\n"
    )
    tmp = dest.with_suffix(".tmp")
    tmp.write_text(body, encoding="utf-8")
    os.chmod(tmp, 0o755)
    os.replace(tmp, dest)


class PeripheralSupervisor:
    def __init__(
        self,
        run_dir: Path,
        control_sock: Path | None = None,
        control_port: int = 0,
        real_whisper_bin: str = "/usr/local/bin/whisper-cli",
        calibration_path: Path | None = None,
        unprovisioned_servos: bool = False,
        with_fake_nmcli: bool = False,
        ready_file: Path | None = None,
        capture_dir: Path | None = None,
    ) -> None:
        self.run_dir = Path(run_dir).resolve()
        self.run_dir.mkdir(parents=True, exist_ok=True)
        self.dev_dir = self.run_dir / "dev"
        self.bin_dir = self.run_dir / "bin"
        self.ipc_dir = self.run_dir / "ipc"
        self.nmcli_dir = self.run_dir / "nmcli"
        for d in (self.dev_dir, self.bin_dir, self.ipc_dir):
            d.mkdir(parents=True, exist_ok=True)

        self.started_mono = time.monotonic()
        self.control_sock = Path(control_sock) if control_sock else (self.run_dir / "control.sock")
        self.ready_file = Path(ready_file) if ready_file else (self.run_dir / "ready.json")
        self.peripherals_file = self.run_dir / "peripherals.json"
        self.with_fake_nmcli = bool(with_fake_nmcli)

        self._stop = threading.Event()

        # Calibration copy/generation
        target_cal = self.run_dir / "neck.json"
        cal_override = None
        if calibration_path is not None:
            cal_override = json.loads(Path(calibration_path).read_text(encoding="utf-8"))

        # Start peripheral emulators
        self.face = FacePty(self.dev_dir / "face")
        self.servo = ServoPty(
            self.dev_dir / "servo",
            target_cal,
            calibration=cal_override,
            unprovisioned=unprovisioned_servos,
        )
        # Keep calibration.json symlink alias beside neck.json for convenience
        cal_alias = self.run_dir / "calibration.json"
        if cal_alias.exists() or cal_alias.is_symlink():
            cal_alias.unlink()
        os.symlink(target_cal.name, cal_alias)

        self.camera = CameraBridge(self.ipc_dir / "camera.sock")
        self.audio = AudioBridge(
            speaker_sock=self.ipc_dir / "speaker.sock",
            mic_sock=self.ipc_dir / "mic.sock",
            whisper_sock=self.ipc_dir / "whisper.sock",
            real_whisper_bin=real_whisper_bin,
            capture_dir=capture_dir,
        )

        # Install env_clear()-safe launchers in <run_dir>/bin
        shims_dir = THIS_DIR / "shims"
        _write_launcher(
            self.bin_dir / "rpicam-vid", shims_dir / "rpicam_vid.py", "--sock", self.camera.sock_path
        )
        _write_launcher(
            self.bin_dir / "aplay", shims_dir / "aplay.py", "--sock", self.audio.speaker_sock
        )
        _write_launcher(
            self.bin_dir / "arecord", shims_dir / "arecord.py", "--sock", self.audio.mic_sock
        )
        _write_launcher(
            self.bin_dir / "whisper-slow",
            shims_dir / "whisper_slow.py",
            "--sock",
            self.audio.whisper_sock,
        )
        if self.with_fake_nmcli:
            self.nmcli_dir.mkdir(parents=True, exist_ok=True)
            (self.nmcli_dir / "state").write_text("connected\n", encoding="utf-8")
            _write_launcher(
                self.bin_dir / "nmcli", shims_dir / "nmcli_stub.py", "--state-dir", self.nmcli_dir
            )

        # Start Unix domain socket JSON-RPC server
        if self.control_sock.exists():
            self.control_sock.unlink()
        self._unix_srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._unix_srv.bind(str(self.control_sock))
        os.chmod(self.control_sock, 0o660)
        self._unix_srv.listen(16)
        self._unix_srv.settimeout(0.2)
        self._unix_thread = threading.Thread(
            target=self._unix_accept_loop, name="control-unix", daemon=True
        )
        self._unix_thread.start()

        # Start loopback HTTP server
        self._http_srv = self._start_http_server(control_port)
        self.control_port = int(self._http_srv.server_address[1])
        self.control_url = f"http://127.0.0.1:{self.control_port}"
        self._http_thread = threading.Thread(
            target=self._http_srv.serve_forever,
            kwargs={"poll_interval": 0.2},
            name="control-http",
            daemon=True,
        )
        self._http_thread.start()

        self.manifest = self._build_manifest()
        self._write_manifest(self.ready_file)
        if self.peripherals_file.resolve() != self.ready_file.resolve():
            self._write_manifest(self.peripherals_file)

    def _build_manifest(self) -> dict[str, Any]:
        env_map = {
            "BUDDY_FACE_SERIAL": str(self.face.link_path),
            "BUDDY_SERVO_PORT": str(self.servo.link_path),
            "BUDDY_SERVO_CALIBRATION": str(self.servo.calibration_path),
            "BUDDY_VISION_CAMERA": str(self.bin_dir / "rpicam-vid"),
            "BUDDY_LISTEN_WHISPER": str(self.bin_dir / "whisper-slow"),
            "BUDDY_LISTEN_DEVICE": "virtual-mic",
            "BUDDY_ALSA_DEVICE": "virtual-speaker",
        }
        paths = {
            "face_pty": str(self.face.link_path),
            "servo_pty": str(self.servo.link_path),
            "calibration": str(self.servo.calibration_path),
            "rpicam_vid": str(self.bin_dir / "rpicam-vid"),
            "aplay": str(self.bin_dir / "aplay"),
            "arecord": str(self.bin_dir / "arecord"),
            "whisper_slow": str(self.bin_dir / "whisper-slow"),
        }
        if self.with_fake_nmcli:
            paths["nmcli"] = str(self.bin_dir / "nmcli")
        return {
            "pid": os.getpid(),
            "run_dir": str(self.run_dir),
            "face_serial": str(self.face.link_path),
            "servo_port": str(self.servo.link_path),
            "servo_calibration": str(self.servo.calibration_path),
            "bin_dir": str(self.bin_dir),
            "camera_bin": str(self.bin_dir / "rpicam-vid"),
            "whisper_bin": str(self.bin_dir / "whisper-slow"),
            "control_url": self.control_url,
            "control_sock": str(self.control_sock),
            "with_fake_nmcli": self.with_fake_nmcli,
            "env": env_map,
            "paths": paths,
        }

    def _write_manifest(self, dest: Path) -> None:
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_suffix(".tmp")
        tmp.write_text(json.dumps(self.manifest, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, dest)

    def nmcli_status(self) -> dict[str, Any]:
        state_file = self.nmcli_dir / "state"
        log_file = self.nmcli_dir / "log.jsonl"
        logs = []
        if log_file.is_file():
            for line in log_file.read_text(encoding="utf-8").splitlines()[-100:]:
                try:
                    logs.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
        return {
            "enabled": self.with_fake_nmcli,
            "state": state_file.read_text(encoding="utf-8").strip() if state_file.is_file() else None,
            "calls": logs,
        }

    def nmcli_set_state(self, state: str) -> dict[str, Any]:
        self.nmcli_dir.mkdir(parents=True, exist_ok=True)
        (self.nmcli_dir / "state").write_text(str(state).strip() + "\n", encoding="utf-8")
        return {"state": str(state).strip()}

    # =========================================================================
    # Unified RPC dispatcher
    # =========================================================================

    def dispatch(self, method: str, params: dict[str, Any] | None = None) -> Any:
        p = params or {}
        if method == "ping":
            return {
                "ok": True,
                "pid": os.getpid(),
                "uptime_ms": round((time.monotonic() - self.started_mono) * 1000.0, 1),
                "run_dir": str(self.run_dir),
                "control_url": self.control_url,
                "control_sock": str(self.control_sock),
            }
        if method == "status":
            return {
                "manifest": self.manifest,
                "face": self.face.status(),
                "servo": self.servo.status(),
                "camera": self.camera.status(),
                "speaker": self.audio.speaker_status(),
                "mic": self.audio.mic_status(),
                "whisper": self.audio.whisper_status(),
                "nmcli": self.nmcli_status(),
            }
        if method == "shutdown":
            self._stop.set()
            return {"stopping": True}

        # ---- Face ----
        if method == "face.status":
            return self.face.status()
        if method == "face.history":
            return self.face.history(
                since_seq=int(p.get("since_seq", 0)),
                limit=int(p.get("limit", 500)),
            )
        if method == "face.inject_hello":
            raw_ver = p.get("version", 1)
            ver = int(float(raw_ver)) if isinstance(raw_ver, (int, float, str)) else 1
            return self.face.inject_hello(
                version=ver,
                eyes=int(p.get("eyes", 1)),
                fragmented=bool(p.get("fragmented", True)),
                noise_prefix=str(p.get("noise_prefix", "")),
            )
        if method == "face.assert_qr_after_hello":
            return self.face.assert_qr_after_hello(
                hello_seq=p.get("hello_seq"),
                expected_qr=p.get("expected_qr"),
                expected_left=p.get("expected_left"),
                expected_right=p.get("expected_right"),
                require_non_empty=bool(p.get("require_non_empty", False)),
                expected_width=p.get("expected_width"),
                timeout_ms=int(p.get("timeout_ms", 3000)),
                forbid_gesture_replay=bool(p.get("forbid_gesture_replay", True)),
            )
        if method == "face.wait_for":
            pred = p.get("predicate", p)
            return self.face.wait_for(pred, timeout_ms=int(p.get("timeout_ms", 5000)))
        if method == "face.set_heartbeat":
            return self.face.set_heartbeat(
                enabled=p.get("enabled"),
                interval_ms=p.get("interval_ms"),
                fps=p.get("fps"),
                link=p.get("link"),
            )
        if method == "face.inject_error":
            return self.face.inject_error(str(p.get("err", "bad json")))
        if method == "face.set_fault":
            return self.face.set_fault(
                stall_writes=p.get("stall_writes"),
                auto_error_reply=p.get("auto_error_reply"),
                reset_termios=bool(p.get("reset_termios", False)),
                reopen_pty=bool(p.get("reopen_pty", False)),
            )
        if method == "face.clear_history":
            return self.face.clear_history()

        # ---- Servo ----
        if method == "servo.status":
            return self.servo.status()
        if method == "servo.history":
            return self.servo.history(
                kind=str(p.get("kind", "all")),
                since_seq=int(p.get("since_seq", 0)),
                limit=int(p.get("limit", 500)),
            )
        if method == "servo.set_register":
            return self.servo.set_register(
                servo_id=int(p["id"]),
                reg=int(p["reg"]),
                data=list(p["bytes"]),
            )
        if method == "servo.set_fault":
            return self.servo.set_fault(
                drop_replies_for=p.get("drop_replies_for"),
                corrupt_checksum=p.get("corrupt_checksum"),
                status_error_byte=p.get("status_error_byte"),
                echo_enabled=p.get("echo_enabled"),
                response_delay_ms=p.get("response_delay_ms"),
                forget_write=p.get("forget_write"),
                clear_forget_write=bool(p.get("clear_forget_write", False)),
            )
        if method == "servo.wait_for_goal":
            return self.servo.wait_for_goal(
                axis=p.get("axis"),
                min_delta_steps=p.get("min_delta_steps"),
                baseline_steps=p.get("baseline_steps"),
                target_steps=p.get("target_steps"),
                tolerance=int(p.get("tolerance", 8)),
                torque=p.get("torque"),
                after_seq=int(p.get("after_seq", 0)),
                timeout_ms=int(p.get("timeout_ms", 5000)),
            )
        if method == "servo.assert_bounds":
            return self.servo.assert_bounds(since_seq=int(p.get("since_seq", 0)))
        if method == "servo.clear_history":
            return self.servo.clear_history()

        # ---- Camera ----
        if method == "camera.status":
            return self.camera.status()
        if method == "camera.set_scene":
            return self.camera.set_scene(
                kind=str(p.get("kind", "blank")),
                path=p.get("path"),
                path_b=p.get("path_b"),
                offset_x=float(p.get("offset_x", 0)),
                offset_y=float(p.get("offset_y", 0)),
                fps=p.get("fps"),
                truncate_after_frames=p.get("truncate_after_frames"),
            )

        # ---- Speaker ----
        if method == "speaker.status":
            return self.audio.speaker_status()
        if method == "speaker.wait_for":
            return self.audio.speaker_wait_for(
                event=str(p.get("event", "end")),
                min_bytes=int(p.get("min_bytes", 0)),
                after_session=int(p.get("after_session", 0)),
                timeout_ms=int(p.get("timeout_ms", 10000)),
            )
        if method == "speaker.set_fault":
            return self.audio.speaker_set_fault(
                pace_realtime=p.get("pace_realtime"),
                stall_after_bytes=p.get("stall_after_bytes"),
                exit_early_code=p.get("exit_early_code"),
                clear_faults=bool(p.get("clear_faults", False)),
            )
        if method == "speaker.set_capture_dir":
            return self.audio.speaker_set_capture_dir(p.get("dir"))
        if method == "speaker.clear_history":
            return self.audio.speaker_clear()

        # ---- Mic & Echo ----
        if method == "mic.status":
            return self.audio.mic_status()
        if method == "mic.play_file":
            return self.audio.mic_play_file(
                path=str(p["path"]),
                fmt=p.get("format"),
                sample_rate=int(p.get("sample_rate", 16000)),
                channels=int(p.get("channels", 1)),
                source_channel=int(p.get("source_channel", 0)),
                gain=float(p.get("gain", 1.0)),
                clip_id=p.get("clip_id"),
                pad_silence_before_ms=int(p.get("pad_silence_before_ms", 200)),
                pad_silence_after_ms=int(p.get("pad_silence_after_ms", 1200)),
                target_channel=int(p.get("target_channel", 0)),
                replace=bool(p.get("replace", False)),
            )
        if method == "mic.play_pcm_b64":
            return self.audio.mic_play_pcm_b64(
                pcm_b64=str(p["pcm_b64"]),
                gain=float(p.get("gain", 1.0)),
                clip_id=p.get("clip_id"),
                pad_silence_before_ms=int(p.get("pad_silence_before_ms", 200)),
                pad_silence_after_ms=int(p.get("pad_silence_after_ms", 1200)),
                target_channel=int(p.get("target_channel", 0)),
                replace=bool(p.get("replace", False)),
            )
        if method == "mic.stop":
            return self.audio.mic_stop()
        if method == "mic.set_noise":
            return self.audio.mic_set_noise(float(p.get("rms", 0.0)))
        if method in ("mic.set_echo", "audio.set_echo"):
            return self.audio.set_echo(
                enabled=p.get("enabled"),
                gain=p.get("gain"),
                delay_ms=p.get("delay_ms"),
                file_path=p.get("file_path"),
                clear_file_path=bool(p.get("clear_file_path", False)),
                inject_now=bool(p.get("inject_now", False)),
            )
        if method == "mic.set_fault":
            return self.audio.mic_set_fault(
                stall=p.get("stall"),
                exit_code=p.get("exit_code"),
            )
        if method == "mic.wait_idle":
            return self.audio.mic_wait_idle(timeout_ms=int(p.get("timeout_ms", 30000)))

        # ---- Whisper ----
        if method == "whisper.status":
            return self.audio.whisper_status()
        if method == "whisper.configure":
            return self.audio.whisper_configure(
                real_whisper_bin=p.get("real_whisper_bin"),
                delay_ms=p.get("delay_ms"),
                delay_before_ms=p.get("delay_before_ms"),
                delay_after_ms=p.get("delay_after_ms"),
                hold=p.get("hold"),
                exit_code=p.get("exit_code"),
                clear_exit_code=bool(p.get("clear_exit_code", False)),
            )
        if method == "whisper.release":
            return self.audio.whisper_release(call_id=p.get("call_id"))
        if method == "whisper.wait_for":
            return self.audio.whisper_wait_for(
                event=str(p.get("event", "started")),
                after_id=int(p.get("after_id", 0)),
                timeout_ms=int(p.get("timeout_ms", 30000)),
            )
        if method == "whisper.clear_history":
            return self.audio.whisper_clear()

        # ---- nmcli ----
        if method == "nmcli.status":
            return self.nmcli_status()
        if method == "nmcli.set_state":
            return self.nmcli_set_state(str(p.get("state", "connected")))

        raise ValueError(f"unknown RPC method: {method}")

    # =========================================================================
    # Unix JSON-RPC transport
    # =========================================================================

    def _unix_accept_loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._unix_srv.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(
                target=self._serve_unix_client,
                args=(conn,),
                name="control-unix-client",
                daemon=True,
            ).start()

    def _serve_unix_client(self, conn: socket.socket) -> None:
        buf = bytearray()
        try:
            conn.settimeout(None)
            while not self._stop.is_set():
                chunk = conn.recv(65536)
                if not chunk:
                    break
                buf.extend(chunk)
                while True:
                    nl = buf.find(b"\n")
                    if nl < 0:
                        break
                    raw = bytes(buf[:nl])
                    del buf[: nl + 1]
                    if not raw.strip():
                        continue
                    req_id = None
                    try:
                        req = json.loads(raw.decode("utf-8"))
                        req_id = req.get("id")
                        method = str(req["method"])
                        params = req.get("params")
                        result = self.dispatch(method, params)
                        resp = {"id": req_id, "ok": True, "result": result}
                    except Exception as exc:
                        resp = {
                            "id": req_id,
                            "ok": False,
                            "error": f"{type(exc).__name__}: {exc}",
                        }
                    conn.sendall((json.dumps(resp, separators=(",", ":")) + "\n").encode("utf-8"))
        except OSError:
            pass
        finally:
            try:
                conn.close()
            except OSError:
                pass

    # =========================================================================
    # Loopback HTTP transport
    # =========================================================================

    def _start_http_server(self, port: int) -> http.server.ThreadingHTTPServer:
        sup = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format: str, *args: Any) -> None:
                return

            def _send_json(self, status: int, body: Any) -> None:
                raw = (json.dumps(body, separators=(",", ":")) + "\n").encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(raw)

            def _read_json_body(self) -> dict[str, Any]:
                length = int(self.headers.get("Content-Length", "0") or "0")
                if length <= 0:
                    return {}
                raw = self.rfile.read(length)
                if not raw.strip():
                    return {}
                val = json.loads(raw.decode("utf-8"))
                if not isinstance(val, dict):
                    raise ValueError("JSON body must be an object")
                return val

            def do_GET(self) -> None:
                parsed = urllib.parse.urlsplit(self.path)
                qs = urllib.parse.parse_qs(parsed.query)
                path = parsed.path

                def qint(key: str, default: int) -> int:
                    return int(qs[key][0]) if key in qs and qs[key] else default

                def qstr(key: str, default: str) -> str:
                    return str(qs[key][0]) if key in qs and qs[key] else default

                try:
                    if path == "/health":
                        self._send_json(200, sup.dispatch("ping"))
                    elif path == "/status":
                        self._send_json(200, sup.dispatch("status"))
                    elif path == "/face/events":
                        self._send_json(
                            200,
                            sup.dispatch(
                                "face.history",
                                {"since_seq": qint("since_seq", 0), "limit": qint("limit", 500)},
                            ),
                        )
                    elif path == "/face/status":
                        self._send_json(200, sup.dispatch("face.status"))
                    elif path == "/servo/events":
                        self._send_json(
                            200,
                            sup.dispatch(
                                "servo.history",
                                {
                                    "kind": qstr("kind", "all"),
                                    "since_seq": qint("since_seq", 0),
                                    "limit": qint("limit", 500),
                                },
                            ),
                        )
                    elif path == "/servo/state":
                        self._send_json(200, sup.dispatch("servo.status"))
                    elif path == "/camera/status":
                        self._send_json(200, sup.dispatch("camera.status"))
                    elif path == "/speaker/sessions":
                        self._send_json(200, sup.dispatch("speaker.status"))
                    elif path == "/mic/status":
                        self._send_json(200, sup.dispatch("mic.status"))
                    elif path == "/whisper/status":
                        self._send_json(200, sup.dispatch("whisper.status"))
                    elif path == "/nmcli/status":
                        self._send_json(200, sup.dispatch("nmcli.status"))
                    else:
                        self._send_json(404, {"ok": False, "error": f"not found: {path}"})
                except Exception as exc:
                    self._send_json(500, {"ok": False, "error": f"{type(exc).__name__}: {exc}"})

            def do_POST(self) -> None:
                path = urllib.parse.urlsplit(self.path).path
                try:
                    body = self._read_json_body()
                    if path == "/rpc":
                        res = sup.dispatch(str(body["method"]), body.get("params"))
                        self._send_json(200, {"ok": True, "result": res})
                        return
                    if path == "/mic/inject":
                        method = "mic.play_pcm_b64" if "pcm_b64" in body else "mic.play_file"
                        self._send_json(200, sup.dispatch(method, body))
                        return
                    route_map = {
                        "/face/hello": "face.inject_hello",
                        "/face/assert-qr-after-hello": "face.assert_qr_after_hello",
                        "/face/wait": "face.wait_for",
                        "/face/heartbeat": "face.set_heartbeat",
                        "/face/error": "face.inject_error",
                        "/face/fault": "face.set_fault",
                        "/face/clear": "face.clear_history",
                        "/servo/register": "servo.set_register",
                        "/servo/fault": "servo.set_fault",
                        "/servo/wait-goal": "servo.wait_for_goal",
                        "/servo/assert-bounds": "servo.assert_bounds",
                        "/servo/clear": "servo.clear_history",
                        "/camera/scene": "camera.set_scene",
                        "/speaker/wait": "speaker.wait_for",
                        "/speaker/fault": "speaker.set_fault",
                        "/speaker/capture": "speaker.set_capture_dir",
                        "/speaker/clear": "speaker.clear_history",
                        "/mic/stop": "mic.stop",
                        "/mic/noise": "mic.set_noise",
                        "/mic/fault": "mic.set_fault",
                        "/mic/wait-idle": "mic.wait_idle",
                        "/audio/echo": "audio.set_echo",
                        "/whisper/config": "whisper.configure",
                        "/whisper/release": "whisper.release",
                        "/whisper/wait": "whisper.wait_for",
                        "/whisper/clear": "whisper.clear_history",
                        "/nmcli/state": "nmcli.set_state",
                        "/shutdown": "shutdown",
                    }
                    method = route_map.get(path)
                    if method is None:
                        self._send_json(404, {"ok": False, "error": f"not found: {path}"})
                        return
                    self._send_json(200, sup.dispatch(method, body))
                except TimeoutError as exc:
                    self._send_json(408, {"ok": False, "error": str(exc)})
                except AssertionError as exc:
                    self._send_json(422, {"ok": False, "error": str(exc)})
                except (KeyError, ValueError) as exc:
                    self._send_json(400, {"ok": False, "error": str(exc)})
                except Exception as exc:
                    self._send_json(500, {"ok": False, "error": f"{type(exc).__name__}: {exc}"})

        srv = http.server.ThreadingHTTPServer(("127.0.0.1", int(port)), Handler)
        srv.daemon_threads = True
        return srv

    def wait_until_stopped(self) -> None:
        while not self._stop.is_set():
            self._stop.wait(timeout=0.5)

    def close(self) -> None:
        self._stop.set()
        try:
            self._http_srv.shutdown()
            self._http_srv.server_close()
        except Exception:
            pass
        try:
            self._unix_srv.close()
        except OSError:
            pass
        self._unix_thread.join(timeout=1.5)
        self._http_thread.join(timeout=1.5)
        self.audio.close()
        self.camera.close()
        self.servo.close()
        self.face.close()
        try:
            if self.control_sock.exists():
                self.control_sock.unlink()
        except OSError:
            pass


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="ARM64 virtual Pip peripheral supervisor")
    p.add_argument(
        "--run-dir",
        "--state-dir",
        dest="run_dir",
        required=True,
        help="Isolated runtime directory for PTYs, shims, sockets, and calibration",
    )
    p.add_argument(
        "--control-sock",
        default=None,
        help="Unix control socket path (default: <run_dir>/control.sock)",
    )
    p.add_argument(
        "--control-port",
        type=int,
        default=0,
        help="Loopback HTTP control port (default: 0 for ephemeral)",
    )
    p.add_argument(
        "--real-whisper",
        default="/usr/local/bin/whisper-cli",
        help="Path to the real ARM64 whisper-cli binary wrapped by whisper-slow",
    )
    p.add_argument(
        "--calibration",
        default=None,
        help="Optional path to custom 3-axis neck calibration JSON",
    )
    p.add_argument(
        "--unprovisioned-servos",
        action="store_true",
        help="Initialize virtual STS3215 EEPROM limits to [0, 4095] to test provisioning",
    )
    p.add_argument(
        "--with-fake-nmcli",
        action="store_true",
        help="Install controllable stub nmcli in <run_dir>/bin/nmcli (default state: connected)",
    )
    p.add_argument(
        "--ready-file",
        default=None,
        help="Path to write JSON ready manifest (default: <run_dir>/ready.json)",
    )
    p.add_argument(
        "--capture-dir",
        default=None,
        help="Optional directory to record raw speaker PCM captures",
    )
    return p.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    sup = PeripheralSupervisor(
        run_dir=Path(args.run_dir),
        control_sock=Path(args.control_sock) if args.control_sock else None,
        control_port=args.control_port,
        real_whisper_bin=args.real_whisper,
        calibration_path=Path(args.calibration) if args.calibration else None,
        unprovisioned_servos=args.unprovisioned_servos,
        with_fake_nmcli=args.with_fake_nmcli,
        ready_file=Path(args.ready_file) if args.ready_file else None,
        capture_dir=Path(args.capture_dir) if args.capture_dir else None,
    )

    def _on_signal(_sig: int, _frame: object) -> None:
        sup._stop.set()

    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGINT, _on_signal)

    # Emit one compact JSON ready line to stdout for callers waiting on startup.
    sys.stdout.write(json.dumps(sup.manifest, separators=(",", ":")) + "\n")
    sys.stdout.flush()

    try:
        sup.wait_until_stopped()
    finally:
        sup.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
