# ARM64 Virtual Pip Peripherals (`tests/pip-e2e/peripherals/`)

Linux ARM64 pseudo-devices and controllable peripheral shims for Release QA of `robot-buddy-robot` (hardware mode, **no `--simulate`**).

## Components

| File | Peripheral / Role | How `robot-buddy-robot` interacts with it |
| --- | --- | --- |
| `supervisor.py` (`daemon.py` alias) | Master peripheral supervisor | Owns PTYs, IPC sockets, drop-in shims, and dual control servers (`<run_dir>/control.sock` JSON-RPC + loopback HTTP `control_url`). Writes `<run_dir>/ready.json` & `<run_dir>/peripherals.json`. |
| `face_pty.py` | Virtual ESP32-S3 face serial board | Real Linux PTY (`<run_dir>/dev/face`) initialized to `cdc_acm` defaults (`9600`, cooked, `ECHO`, `HUPCL`, `-CLOCAL`). Verifies `open_serial()` applies `115200 raw -echo clocal -hupcl`, validates JSON lines (`led`, `e`/`i`, `look`, `g`, `ping`, `qr`), sends heartbeats, injects soft-reset `{"hello":"pip-face",...}` **without closing the PTY fd**, and asserts QR readback + no gesture replay after hello. |
| `servo_pty.py` | Virtual Feetech STS3215 3-axis neck bus | Real Linux PTY (`<run_dir>/dev/servo`) + validated calibration (`<run_dir>/neck.json`). Verifies Linux 1 Mbaud (`B1000000`) raw termios, emulates half-duplex echo, implements PING/READ/WRITE/SYNC WRITE (`0x83`), enforces EEPROM lock (`reg 55 == 0` required to write regs `< 40`), checks goal bounds against calibrated & EEPROM limits (`reg 9..13`), tracks present position (`reg 56..58`) when `TORQUE == 1`, and records 50 Hz cadence + violations. |
| `camera_bridge.py` + `shims/rpicam_vid.py` | Simulated Pi 5 CSI camera (`rpicam-vid`) | Installed at `<run_dir>/bin/rpicam-vid` (`BUDDY_VISION_CAMERA`). Survives `vision/mod.rs` `cmd.env_clear().env("PATH", "/usr/bin:/bin")`. Streams exact 384×288 packed I420 (`165,888` bytes/frame) for real OpenCV YuNet + SFace inference with dynamic scene switching (`blank`, `i420`, `ppm`, `two_faces`, `stall`, `disconnect`, `truncate_after_frames`, normalized/pixel `offset_x`/`offset_y`, `--rotation 180`). |
| `audio_bridge.py` + `shims/aplay.py` | Fake ALSA speaker (`aplay`) | Installed on `PATH` at `<run_dir>/bin/aplay`. Consumes 24 kHz mono S16_LE PCM, paces playback realistically, detects `SIGKILL` barge-in immediately via socket EOF, records session telemetry + optional raw PCM captures, and feeds live 24 kHz → 16 kHz resampled acoustic echo into the virtual mic when enabled. |
| `audio_bridge.py` + `shims/arecord.py` | Fake ALSA mic (`arecord`) | Installed on `PATH` at `<run_dir>/bin/arecord`. Survives `listen/io.rs` `env_clear()`. Streams 20 ms (`320`-sample) 16 kHz S16_LE PCM frames from real recorded `.wav`/`.pcm` fixtures, mixed with configurable ambient noise and live speaker echo, routed to `target_channel` of `-c <channels>`. |
| `audio_bridge.py` + `shims/whisper_slow.py` | Slow real Whisper wrapper | Installed at `<run_dir>/bin/whisper-slow` (`BUDDY_LISTEN_WHISPER`). Survives `listen/io.rs` `env_clear()`. Delegates transparently to the **real ARM64 `whisper-cli`** binary (never faking transcripts or rewriting argv), adds controllable `delay_before_ms` / `delay_after_ms` / `hold` + `whisper.release`, tracks mid-decode `SIGKILL` cancellation, and sets Linux `PR_SET_PDEATHSIG(SIGKILL)` on child `whisper-cli` so zero orphans remain. |
| `shims/nmcli_stub.py` | Optional fake `nmcli` (`--with-fake-nmcli`) | Controllable `nmcli` stub (default `connected`) for non-`--simulate` container runs and isolated Wi-Fi setup QR testing. |
| `client.mjs`, `client.py`, `ctl.py` | Zero-dependency IPC clients | ESM client for Bun/Node (`runner*.mjs`), Python client, and CLI tool. |
| `fetch-vision-models.sh` + `bake_i420.py` | Vision model & fixture fetcher | Downloads SHA-256-verified official OpenCV Zoo YuNet + SFace ONNX models (pinned commit `48b10fa505aeb51fbd127fc60a5c660040e823b7`) and Apache-2.0 OpenCV test face portraits (`opencv/opencv_extra@0fd5515efaa81195465b04fa6e791922de3068e2`) into gitignored cache (`tests/pip-e2e/.cache/vision`) and bakes 384×288 packed I420 frames. |
| `test_peripherals.py` + `run-peripherals-tests.sh` | Linux ARM64 fixture qualification suite | Verifies all pseudo-devices, IPC transports, real OpenCV YuNet tracking, real ARM64 `whisper-cli`, real `robot-buddy-robot servo` CLI, and real Wi-Fi setup QR readback after soft-reset `hello` without closing the PTY fd. |

## Quick Usage

### 1. Populate vision models & I420 face frames (gitignored cache)
```sh
tests/pip-e2e/peripherals/fetch-vision-models.sh tests/pip-e2e/.cache/vision
```

### 2. Start the peripheral supervisor
```sh
python3 tests/pip-e2e/peripherals/supervisor.py \
  --state-dir /tmp/pip-run \
  --control-port 0 \
  --real-whisper /opt/pip-e2e/speech/bin/whisper-cli \
  --with-fake-nmcli
```
Wait for `/tmp/pip-run/ready.json` (also written at `/tmp/pip-run/peripherals.json`). Prepend `bin_dir` to `PATH` and apply `env` to `robot-buddy-robot`.

### 3. Run the ARM64 peripheral fixture test suite
```sh
tests/pip-e2e/peripherals/run-peripherals-tests.sh
```
Output report: `tests/pip-e2e/.artifacts/peripherals-fixture-report.json` (gitignored).

## IPC Reference

Available over both `<run_dir>/control.sock` (newline-delimited JSON-RPC: `{"id":1,"method":"...","params":{...}}\n`) and `control_url` (`http://127.0.0.1:<port>`):

- **General**: `ping` (`GET /health`), `status` (`GET /status`), `POST /rpc`
- **Face PTY**: `face.status` (`GET /face/status`), `face.history` (`GET /face/events`), `face.inject_hello` (`POST /face/hello`), `face.assert_qr_after_hello` (`POST /face/assert-qr-after-hello`), `face.wait_for` (`POST /face/wait`), `face.set_heartbeat` (`POST /face/heartbeat`), `face.inject_error` (`POST /face/error`), `face.set_fault` (`POST /face/fault`), `face.clear_history` (`POST /face/clear`)
- **Servo PTY**: `servo.status` (`GET /servo/state`), `servo.history` (`GET /servo/events`), `servo.wait_for_goal` (`POST /servo/wait-goal`), `servo.set_register` (`POST /servo/register`), `servo.set_fault` (`POST /servo/fault`), `servo.assert_bounds` (`POST /servo/assert-bounds`), `servo.clear_history` (`POST /servo/clear`)
- **Camera**: `camera.status` (`GET /camera/status`), `camera.set_scene` (`POST /camera/scene`)
- **Speaker**: `speaker.status` (`GET /speaker/sessions`), `speaker.wait_for` (`POST /speaker/wait`), `speaker.set_fault` (`POST /speaker/fault`), `speaker.set_capture_dir` (`POST /speaker/capture`), `speaker.clear_history` (`POST /speaker/clear`)
- **Mic & Echo**: `mic.status` (`GET /mic/status`), `mic.play_file` / `mic.play_pcm_b64` (`POST /mic/inject`), `mic.stop` (`POST /mic/stop`), `mic.set_noise` (`POST /mic/noise`), `mic.set_echo` / `audio.set_echo` (`POST /audio/echo`), `mic.set_fault` (`POST /mic/fault`), `mic.wait_idle` (`POST /mic/wait-idle`)
- **Slow Whisper**: `whisper.status` (`GET /whisper/status`), `whisper.configure` (`POST /whisper/config`), `whisper.release` (`POST /whisper/release`), `whisper.wait_for` (`POST /whisper/wait`), `whisper.clear_history` (`POST /whisper/clear`)
- **nmcli** (opt-in): `nmcli.status` (`GET /nmcli/status`), `nmcli.set_state` (`POST /nmcli/state`)

## Explicit Boundary: Fixtures vs. Real Hardware

**What is real in these tests:**
- Real Linux ARM64 kernel PTYs and `termios` (`115200 raw -echo clocal -hupcl` on face; `B1000000` 1 Mbaud raw on STS3215).
- Real ARM64 release `robot-buddy-robot` binary (`--features pi-vision,onnx-wake`, **without `--simulate`**).
- Real OpenCV 4.10 YuNet + SFace ONNX CPU inference on pinned public OpenCV test portraits.
- Real ARM64 `whisper-cli` (`whisper.cpp v1.9.6`) + `ggml-tiny.en.bin` on real recorded human speech WAVs.
- Real `env_clear()` child-process isolation and `kill_on_drop` cancellation semantics.

**What is simulated / fixture-only (not proven here, still requires physical Pip bench validation):**
- Physical ESP32-S3 USB CDC-ACM electrical disconnects, LCD rendering, and physical eye bezels.
- Physical Feetech STS3215 servo torque load, gear backlash, mechanical endstops, and power-rail brownout.
- Physical Raspberry Pi 5 CSI camera sensor, libcamera ISP, lens focus, and auto-exposure.
- Physical MAX98357A I2S speaker, reSpeaker mic array beamforming, room acoustics, and hardware AEC.
- Real systemd/NetworkManager D-Bus policy and RF Wi-Fi association.
