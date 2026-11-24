# ARM64 Pip E2E & Continuous Soak Harness

Full-stack release qualification harness running the **real ARM64 Linux release binaries** inside a native `linux/arm64` Debian Trixie container:

- `/usr/local/bin/rivet-engine` — pinned `rivetdev/engine:3.1.1@sha256:55cdfe9a92c0ee2f7d3ce5f388dab2ef8e0be1bab8e3f1adb7ba972f862e25d4`
- `/usr/local/bin/robot-buddy-server` — release build (`rust:1.100.0`, shared BuildKit cache `robot-buddy-*`, `-j 2`)
- `/usr/local/bin/robot-buddy-robot` — release build with `--features pi-vision,onnx-wake` (`rust:1.100.0-slim-trixie`, shared BuildKit cache `pi-cargo-*`, `-j 2`)
- **No `--simulate` flag and no control-socket submit shortcut.** `robot-buddy-robot` runs in production hardware service mode over real TLS/WSS against an ephemeral test CA, receiving speech through virtual `arecord` → real VAD → real ARM64 `whisper-cli` + `ggml-tiny.en.bin` (and optional real ONNX Runtime + openWakeWord + read-only mounted `hey_pip.onnx`), driving real Linux PTYs for the ESP32 face and Feetech STS3215 servos, real OpenCV YuNet/SFace vision inference, and virtual `aplay` speaker capture.

## Ownership & Directory Layout

- `tests/pip-e2e/Dockerfile`, `runner.mjs`, `runner-stack.mjs`, `runner-fakes.mjs`, `run-in-docker.sh`, `.gitignore`, `README.md` — system integration, TLS/OIDC/model/speech fakes, fault injection, reports, and continuous soak.
- `tests/pip-e2e/peripherals/**` — ARM64 virtual Pip peripherals (`supervisor.py`, face/servo PTYs, virtual `rpicam-vid`/`aplay`/`arecord`/slow-`whisper`, `fetch-vision-models.sh`).
- `tests/pip-e2e/speech/**` — ARM64 real `whisper-cli` + `ggml-tiny.en.bin` + ONNX Runtime + openWakeWord installation, checksum locks, and speech fixture qualification.
- `tests/pip-e2e/.cache/` and `tests/pip-e2e/.artifacts/` — **gitignored** downloaded official models/binaries and run reports. Never commit models, private weights, recordings, or run artifacts.

## Quick Start (ARM64 Docker)

```sh
# 1. Build the ARM64 E2E image (reuses shared BuildKit Cargo caches, bounded -j 2)
tests/pip-e2e/run-in-docker.sh build

# 2. Populate official checksum-verified speech & vision caches (gitignored)
tests/pip-e2e/run-in-docker.sh prepare-cache

# 3. Fast preflight smoke (~60-90s): enrollment + OIDC + face/servo + real speech + express + shutdown
tests/pip-e2e/run-in-docker.sh run --suite smoke

# 4. Full functional + fault suite (core + TLS proxy drop + lost submit ack + real server & engine 3.1.1 restart)
tests/pip-e2e/run-in-docker.sh run --suite all

# 5. Qualifying 30-minute continuous conversation soak
tests/pip-e2e/run-in-docker.sh run --suite soak --duration 30m --out /opt/pip-e2e/artifacts/soak-30m
```

### Optional Read-Only Private Mounts

Private assets are **never** copied into images, caches, or commits:

- `PIP_E2E_HEY_PIP_ONNX=/path/to/hey_pip.onnx` — mounted read-only at `/opt/pip-e2e/private/hey_pip.onnx` for `--neural-wake` runs.
- `PIP_E2E_HUMAN_SPEECH_DIR=/path/to/human-recordings` — mounted read-only at `/opt/pip-e2e/private/human-speech` for real human-recorded positive/negative wake fixtures.

## Sustained Soak Gate

`soak-gate.mjs` is the pure full-run gate behind the soak check. Over `[start, end]` of the soak it requires:

- **Neck motion:** a `neck loop timing` window every <= 7.5s from start to end (final freshness <= 7.5s), each window's `total_ticks` difference equal to its `ticks` (no counter reset), >= 45 Hz per window, >= 90% in-run tick coverage (pre-soak ticks never count), and zero `neck motion stopped` / `neck disarmed` / `neck worker panicked` / torque-off-unconfirmed lines anywhere in `robot.log`.
- **Vision:** a `camera inference active` window every <= 7.5s with strictly advancing `vision_frames`, fresh at the end, no `camera stopped` in the run.
- **Per-sample peripherals:** every sample has RSS, servo, camera, and `/proc/<pid>/task/*/comm` thread names; servo goal frames advance >= 45 Hz between every consecutive pair; torque is on for all three joints in every sample; `neck-50hz` is present in every sample; a final sample is taken at the end timestamp.
- **Log integrity:** no malformed telemetry lines, no panics. A partial last line is ignored and reported; a log that stops early fails final freshness.

Failures are stored with timestamps and redacted error templates in `soak-report.json` and `soak-gate-evidence.json` (per-window counter differences), both behind the artifact redaction gate. First/mid/end robot RSS and thread names are in the report.

```sh
bun test tests/pip-e2e/soak-gate.test.mjs     # fixture proofs: late stop, 10s gap, truncated log, ...
bun tests/pip-e2e/soak-gate.mjs --robot-log robot.log --telemetry soak-telemetry.json --start <iso> --end <iso>
```

Provenance: the report binds the host git HEAD/dirty paths (passed by `run-in-docker.sh`), a digest of every harness file that ran, the robot/server ELF GNU build-ids, and, when `PIP_E2E_EXPECT_ROBOT_SHA256` is set, whether `robot-buddy-robot` matches it.

## Fixture vs Real Hardware Boundary

Every generated report (`<suite>-report.json` and `<suite>-report.md`) explicitly distinguishes:

- **Real release artifacts exercised:** `robot-buddy-server`, pinned `rivet-engine` 3.1.1, `robot-buddy-robot --features pi-vision,onnx-wake` (non-`--simulate` TLS mode), ARM64 `whisper-cli` + `ggml-tiny.en.bin`, OpenCV YuNet/SFace inference, and optional ONNX Runtime wake inference.
- **Fixture substitutes in this harness:** local mock OpenAI-compatible SSE model, local mock 24 kHz PCM speech provider, local mock RS256 OIDC IdP, Linux PTYs for face/STS3215 serial buses, and virtual `arecord`/`aplay`/`rpicam-vid` shims.
- **Remaining physical Pi 5 hardware gates:** physical STS3215 calibration/direction/cable stops/torque release, physical reSpeaker microphone & MAX98357A speaker echo/SNR, physical OV5647 exposure/lens framing, and Raspberry Pi 5 thermal/voltage behavior under real cgroup limits.
