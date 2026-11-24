# Changelog

## Pip hardware port (`feat/pip-hardware`, compared with `main`)

`main` is at `07e75ce`, which is also the merge base. The last code, test or infra change is `82e8d5b`; later commits are docs and the refreshed benchmark data. Run `git diff --stat main...feat/pip-hardware` for current totals. Most of the volume is tests, harnesses and committed matrix logs (`infra/evidence/`). For details, see `git log --oneline main..feat/pip-hardware`.

### Robot device client (`apps/robot`)

- **Local listening** (`listen/`): ALSA capture, a built-in energy VAD (20 ms frames, 800 ms end), `whisper-cli` transcription, transcript wake ("Hey Pip", "Hi Pip", "Pip"), and optional neural wake through ONNX Runtime and openWakeWord with butty's `hey_pip.onnx`, which is not in this repo (`onnx-wake` feature). Neural wake takes priority. Barge-in uses neural wake only. Fixes cover VAD recovery from sustained noise, reaping capture before reopening exclusive ALSA devices, and whisper.cpp ≥ 1.8 stdout (`-of /dev/null`). (`4230d7d`, `9027fc6`, `f0e88d1`, `1dbe0c3`, `768dc2c`, `0e8f497`, `5351762`)
- **Neck** (`motion/`): calibrated STS3215 protocol at 1 Mbaud, per-device `neck.json` with EEPROM limit verification, the `servo ping|read|verify|provision|center` CLI, a 50 Hz worker with deadline instrumentation (late cycles and skipped 20 ms slots), gestures and expression poses, and hardened status-reply timeouts. (`481033c`, `a1be3fa`, `75e74e9`, `714bd0f`, `9c939a5`, `c5d7c08`)
- **Face** (`face.rs`): drives the full ESP32-S3 firmware protocol, drains board status, repaints mood, gaze and QR after a board reboot, and keeps eye QR codes in view. (`98cea87`, `11d8605`, `4fd3b5a`, `ee6b18f`)
- **Vision** (`vision/`, `pi-vision` feature): `rpicam-vid` at 15 fps, in-process OpenCV YuNet tracking and optional SFace recognition with a private on-device gallery, and camera gaze converted to the eye board's viewer frame. (`2927622`, `425fcf5`, `104bec3`)
- **Operator panel and setup** (`screen/operator.*`, `wifi/`): a loopback operator console on `:8091` for claimed devices (preview, poses, enrollment), hotspot QR codes on the eyes, setup codes cleared on shutdown, and the Wi-Fi helper isolated from operator credentials. (`4a10922`, `8d0502c`, `3a0ea3c`, `eddac23`, `fb5eeff`)
- **Speech out**: a bounded offline Piper announcement as a separate process, playback reaped before idle, unowned speech gated, Piper and aplay hardened, and credentials kept out of audio children. (`bd28f12`, `b72b7b8`, `845af40`, `0f0bc60`)
- **Integration**: local peripherals wired into live conversation and ownership through `hardware.rs` (`550663f`).

### Server and shared crate (`apps/server`, `crates/buddy`)

- New typed, live-only `express` tool and `face.express` output event (emotion, intensity, gesture), never replayed. (`7038b6c`)
- Sleep is reserved for a local device command. (`8292862`)
- OIDC claim flow hardened (cancelled-callback page, discovery checks). (`6c41452`)

### Deployment and infra (`infra/`, `DEPLOYMENT.md`)

- `DEPLOYMENT.md`: full Pi 5 bring-up, with a native Bookworm private-prefix OpenCV 4.10 build, Trixie and Buildx builds, the camera stride check, models and the go/no-go checklist. (`6d7a513`, `3754d45`, `88994f2`, `6c41452`)
- The `robot-buddy-pip.conf` drop-in (video group, `MemoryMax=1G`, `TasksMax=256`) and `infra/pi-build.Dockerfile` (`6d7a513`). Polkit rule at priority 40 and the systemd `SystemCallFilter` fix (`6c41452`). Optional native vision and panel dependencies (`99e579e`).
- Rust 1.100.0 pinned in local, CI and container builds. (`3117729`, `3e7fff6`, `34bd2aa`)
- ARM64 Bookworm/Trixie toolchain matrix (`infra/run-arm64-matrix.sh`, `arm64-matrix.Dockerfile`, `arm64-matrix-check.sh`) and its evidence. (`37a776c`, `48bba8f`)

### Tests, harnesses and CI

- CI runs the full-feature robot unit tests (`c6ca146`). `check:all` gains the `pip full features` step.
- ARM64 recording-replay fixture (`tests/pip-replay-arm64.mjs`) and the Pi 5 acoustic benchmark (`scripts/pip-perf.py`, `tests/pip-perf.mjs`). (`9849bd3`, `bb8753d`)
- ARM64 speech qualification with an asset lock, and the human wake recipe. (`753b29c`, `d65e75a`, `3a291d2`)
- Virtual Pip peripherals: PTY face and servo bus, fake ALSA and camera. (`3a71343`)
- Fresh ARM64 systemd deployment rehearsal (`tests/pip-deploy-qa`). (`7045fc1`, `738db59`, `5dc8a34`)
- Full-stack E2E, fault suite and 30-minute soak (`tests/pip-e2e`), with a sustained soak gate over motion, vision, servo torque and threads. (`0fc184c`, `6500385`, `4b18b0d`, `82e8d5b`)
- Deterministic fixes for parallel-test flakes (PTY backpressure, tracing capture). (`eb639ad`, `7abeb83`, `d7a5b34`, `1d020e4`)

### Docs

- Release qualification gates (`docs/testing-pip-release.md`), performance procedure (`docs/performance-pip.md`), provenance (`NOTICE.md`) and robot-client updates. The e2e latency baseline was republished at `82e8d5b`. (`5c197de`, `0d73c26`, `d4ef544`)
- Handoff docs after `d4ef544` (docs only): the README front door and butty comparison, DEPLOYMENT.md (hardware reference, binary choice, model pins including butty's `hey_pip.onnx`, "Confirm on your Pip" checks), AGENTS.md commands, NOTICE.md provenance, the [docs index](docs/README.md) and this changelog.

## Validation status

Everything below ran **without a physical Pip**. All of it is at `82e8d5b` unless noted. Since then only docs and bench files have changed.

| Check | Result |
| --- | --- |
| 30-minute continuous E2E soak, virtual Pip (`tests/pip-e2e`, real ARM64 robot, server and Rivet 3.1.1, real whisper-cli, `hey_pip.onnx` mounted read-only) | **PASS.** 149/149 turns. 50 Hz neck motion at 99.9% in-run tick coverage, 0 neck stops. Robot RSS peaked at 185.9 MiB and plateaued. The rivet-engine process did **not** plateau: about +2.6 to 3.1 MiB/min per engine instance over the run (one reviewer measured +3.7 over the final 10 minutes) over the run. Robot ELF SHA-256 `be7cae4e42fda047f857d327af875e0dbf91b54af3a8076df84080e802e2768b` (GNU BuildID `c15c88b7594bd0919c5a0bf78609c2b01c6162e9`) from the pip-e2e image recipe. Soak report artifact `soak-30m-c5d7c08`: kept outside the repo; not needed for bring-up. [infra/toolchain-validation.md](infra/toolchain-validation.md) records this hash and BuildID and explains why the matrix's own ELFs differ. Clean rebuilds on 2026-11-24 from `82e8d5b` and from `c5d7c08` (the pip-e2e recipe) and from `82e8d5b` (`infra/pi-build.Dockerfile`) reproduced this ELF byte for byte ([DEPLOYMENT.md §2](DEPLOYMENT.md#2-build-the-linux-arm64-binary)) |
| ARM64 toolchain matrix, Bookworm and Trixie, default and full Pip features ([evidence](infra/toolchain-validation.md)) | **PASS**, no retries. Robot bin 254 passed and 2 ignored with full features |
| Workstation e2e latency bench ([table](docs/research/e2e-latency.md)) | Refreshed. p95 submit → first PCM byte at the player was **699.8 ms** (Apple M4 Pro, `--simulate`, fake model and speech). The clock starts at transcript submission, so it excludes VAD, Whisper and acoustic I/O |
| Fresh ARM64 systemd deployment rehearsal ([evidence](tests/pip-deploy-qa/EVIDENCE.md)) | **PASS** on the earlier candidate `5351762`: install, 1 GiB drop-in, OIDC claim, upgrade and rollback |

**Open: physical-only items.** Each needs the real robot (see the [DEPLOYMENT.md checklist](DEPLOYMENT.md#5-hardware-bring-up-checklist)):

- Natural "Hey Pip" in a real room, across people and playback conditions
- Microphone SNR and echo rejection with the real reSpeaker and MAX98357A speaker
- Servo calibration, direction, cable-safe stops and torque release on the assembled neck
- OV5647 exposure, lens framing and tracking direction
- Pi 5 thermal and voltage behaviour under the real 1 GiB cgroup limits
- Physical acoustic latency: end of speech → first audible, and wake → playback stop

**Also open:** the rivet-engine RSS growth seen in the soak (about +2.6 to 3.1 MiB/min per engine instance over the run (one reviewer measured +3.7 over the final 10 minutes)) is unexplained. Watch engine memory on the real server host over longer runs.
