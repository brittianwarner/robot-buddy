# Raspberry Pi 5 Acoustic Performance & Hardware-Budget Benchmarking

This guide defines the **on-device Raspberry Pi 5 acoustic benchmark** for `robot-buddy-robot` running under the full Pip hardware profile (`infra/robot-buddy.service` + `infra/robot-buddy-pip.conf`, `MemoryMax=1G`, `TasksMax=256`) with **camera inference and the 50 Hz neck loop active**, plus the **ARM64 recording-replay E2E fixture** that verifies the real Linux client paths without physical hardware.

> **Important honesty boundary:** Repository tests, [end-to-end simulation latency](research/e2e-latency.md) (`bun run bench:e2e`), and the ARM64 recording-replay fixture do **not** measure real room acoustics, microphone/I²S hardware buffering, real `whisper.cpp` / ONNX / OpenCV CPU contention, or physical 1 GiB cgroup pressure on a Pi 5. All targets below are **provisional acceptance targets, not measured hardware guarantees**. No repository defaults are changed by this guide or its scripts.

## 1. What this measures (and why simulation is not enough)

`bun run bench:e2e` measures server + Rivet + `--simulate` client overhead against synthetic providers. On a physical Pip, user-perceived latency and stability also include:

1. **End of speech → first audible**: ALSA `arecord` capture → VAD trailing silence (`END_FRAMES = 40` × 20 ms = 800 ms default in `apps/robot/src/listen/vad.rs`) → `whisper-cli` spawn/model load/decode (`-t 2`) → control dispatch → Rivet submit → model TTFT + first sentence → TTS TTFB → HTTP PCM → `aplay` buffer (`BUDDY_ALSA_BUFFER_US=120000`, 120 ms default) → MAX98357A I²S acoustic output.
2. **Wake END → actual playback stop (barge-in)**: End of the spoken `"Hey Pip"` phrase → acoustic detection (`onnx-wake` 80 ms hop or transcript wake) → `Control::Wake` → `SIGKILL` on `aplay` → actual acoustic silence at the speaker (including ALSA/I²S hardware drain).
3. **50 Hz neck motion deadline stability**: Whether concurrent camera inference (`rpicam-vid` + OpenCV YuNet/SFace), `whisper.cpp`, and speech playback cause the 20 ms (`neck-50hz`) STS3215 servo thread to miss deadlines.
4. **1 GiB cgroup memory ceiling**: Combined peak memory (`MemoryPeak` and 250 ms polled `MemoryCurrent`) across the entire `robot-buddy.service` cgroup — the Rust daemon **plus** `rpicam-vid`, `arecord`, `whisper-cli` (or resident `whisper-server`), `aplay`, and `piper`.

## 2. Provisional acceptance targets (not measured guarantees)

Enforced by `scripts/pip-perf.py run` and `scripts/pip-perf.py compare --strict`. These are **provisional engineering targets** to be confirmed or adjusted after physical Pi 5 bench measurements (coordinate any final release-gate adjustments with `DEPLOYMENT.md` and the OIDC/deployment owners):

| Metric / Gate | Provisional Target | Notes |
| --- | --- | --- |
| Minimum sample count | `≥ 20` turns (after `≥ 1` warm-up) and, for `neural` wake, `≥ 10` barge-ins; `0` failed trials | Runs below minimum sample counts are marked **UNQUALIFIED**. Transcript runs default `--barge 0` and refuse contradictory acoustic barge-in samples. |
| Active neck proof (in-window) | `total_ticks > 0` and `0` `neck motion stopped` events | Proven from `robot_buddy_robot::motion` 5 s timing logs, not static config. |
| Active camera proof (in-window) | Positive `vision_frames` delta from `camera inference active` and `0` camera errors | Proven from `robot_buddy_robot::vision` 5 s logs; zero logs or a stalled counter is **UNQUALIFIED**. |
| End of speech → first audible (p95) | `≤ 3000 ms` (`3.0 s`) | Measured acoustically from end of prompt speech to first audible reply onset. |
| Wake END → actual playback stop (p95) / echo immunity | **Neural wake (`--wake-mode neural`)**: `≤ 500 ms` (`0.5 s`) from **wake phrase END** to sustained acoustic silence (`≥ 250 ms`). **Transcript mode (`--wake-mode transcript`)**: acoustic stop is **N/A** (spoken barge-in is intentionally disabled without local neural wake); requires **echo immunity** (`0` spurious `turn.cancelled` interruptions). | Never compare neural barge-in latency against transcript mode; qualify transcript runs on echo immunity instead. |
| 50 Hz neck missed 20 ms slots | `≤ 0.1%` skipped 20 ms slots (`skipped_slots / total_ticks`) | Reports both **late cycles** (`missed`) and **skipped 20 ms slots** (`skipped_slots`). |
| Effective cgroup cap & peak memory | Effective `MemoryMax ≤ 1 GiB` (`1024 MiB`); authoritative kernel `memory.peak` `< effective cap` and `≤ 90%` proposed **10% reserve** (`≤ 921.6 MiB` at 1 GiB) | A higher `MemoryMax` (or unlimited/unknown) is **UNQUALIFIED** (never clamped). Kernel `memory.peak` is required; 250 ms `MemoryCurrent` polling is reported separately as a **lower bound** only. |
| Safety faults, panics, restarts, OOM | `0` OOM kills, `0` restarts (`NRestarts` delta), `0` panics / unconfirmed torque-off / safety faults | Hard failure on any occurrence. |
| Qualification integrity | `allow_inactive=false`, `simulated=false` | `compare --strict` refuses `--allow-inactive`, simulated, or unqualified runs. |

Privacy rule: `scripts/pip-perf.py` never reads or exports values from `/etc/robot-buddy/device.env` (no serial paths, URLs, credentials, or tokens in JSON artifacts), and trial WAV files are deleted immediately after analysis unless `--save-wavs` is explicitly passed.

## 3. Built-in 50 Hz neck timing instrumentation

`apps/robot/src/motion/mod.rs` tracks every 20 ms tick in a fixed-capacity accumulator (zero heap allocation after thread startup) and logs a summary every 5 seconds (and on clean shutdown; never before `drop(drive)` disables torque on an I/O fault):

- **All ticks on time (`INFO`)**:
  ```text
  robot_buddy_robot::motion: neck loop timing period_ms=20 ticks=250 missed=0 skipped_slots=0 total_ticks=250 total_missed=0 total_skipped_slots=0 p95_cycle_us=1840 max_cycle_us=2910 max_work_us=1620 max_overrun_us=0
  ```
- **One or more ticks finished at or after `scheduled + 20 ms` (`WARN`)**:
  ```text
  robot_buddy_robot::motion: neck loop missed 20ms deadline period_ms=20 ticks=250 missed=2 skipped_slots=5 total_ticks=500 total_missed=2 total_skipped_slots=5 p95_cycle_us=3120 max_cycle_us=64800 max_work_us=64100 max_overrun_us=44800
  ```

Field definitions:
- `missed` / `total_missed` (**late cycles**): count of 50 Hz iterations whose `finished` timestamp reached or passed `scheduled + 20 ms`.
- `skipped_slots` / `total_skipped_slots` (**skipped 20 ms slots**): sum of `floor((finished - scheduled) / 20 ms)` across late cycles (`≥ missed`). A single 55 ms stall counts as `1` late cycle and `2` skipped 20 ms slots.
- `p95_cycle_us` / `max_cycle_us`: nearest-rank p95 and max `finished - scheduled` (wake jitter + serial health/goal work).
- `max_work_us`: max `finished - started` (work time excluding thread wake delay).
- `max_overrun_us`: max `finished - (scheduled + 20 ms)` past the 20 ms budget.

## 4. On-device acoustic bench setup

### Hardware rig

Use a **separate reference capture path** — never open Pip's own reSpeaker microphone for measurement while `robot-buddy.service` owns it.

1. **Preferred (2-channel stereo mic + stimulus loopback):** Connect a 2-channel USB audio interface to the bench host (or Pi). Wire channel 0 to a room measurement microphone positioned ~30 cm from Pip's MAX98357A speaker, and channel 1 to a direct line loopback of the bench playback speaker. Stereo mode (`--ref-channels 2`) gives sub-10 ms stimulus end-of-speech / wake-end detection and accurately captures early neural wake stops (even before `"Hey Pip"` finishes).
2. **Minimal (1-channel mono measurement mic):** Plug a separate USB measurement microphone (`--ref-device plughw:CARD=...,DEV=0`, `--ref-channels 1`) and a bench loudspeaker (`--stim-device ...`) to play fixed prompt and wake WAV clips (16 kHz mono S16_LE). Keep prompt WAVs private on the bench machine — do not commit recordings to the repository.

### Temporary benchmark logging drop-in (do not commit to defaults)

To enable the 5 s `INFO` neck/camera telemetry and `DEBUG` first-audio breakdown during benchmarking without logging transcripts or secrets:

```sh
sudo install -d /etc/systemd/system/robot-buddy.service.d
sudo tee /etc/systemd/system/robot-buddy.service.d/89-bench-log.conf >/dev/null <<'EOF'
[Service]
Environment=RUST_LOG=info,robot_buddy_robot::audio=debug
EOF
sudo systemctl daemon-reload
```

Verify before every variant run:
1. `infra/robot-buddy-pip.conf` (`20-pip.conf`) is installed with `MemoryMax=1G`, `TasksMax=256`, `OMP_NUM_THREADS=2`, `OPENBLAS_NUM_THREADS=1`.
2. Camera (`BUDDY_VISION=1` with a face/subject in view so YuNet + SFace execute continuously) and calibrated neck (`BUDDY_SERVO_PORT` + `BUDDY_SERVO_CALIBRATION`) are enabled.
3. Restart the service cleanly right before each variant so kernel `MemoryPeak` starts fresh:
   ```sh
   sudo systemctl restart robot-buddy
   systemctl show robot-buddy -p ActiveState -p MemoryMax -p TasksMax -p NRestarts
   vcgencmd get_throttled   # must be throttled=0x0
   ```

Remove `/etc/systemd/system/robot-buddy.service.d/89-bench-log.conf` (and any variant drop-ins below), run `sudo systemctl daemon-reload && sudo systemctl restart robot-buddy` when done.

## 5. Comparing the three variants under 1 GiB (unchanged defaults)

Run all three variants back-to-back on the same Pi 5, same room position, same prompt/wake WAVs, same backend/network, and same active camera + neck load. Restart `robot-buddy.service` before each run.

### Variant A: `baseline` (unchanged defaults)

Uses the shipped defaults without modification:
- `END_FRAMES = 40` (800 ms VAD trailing silence in `apps/robot/src/listen/vad.rs`)
- Cold per-clip `/usr/local/bin/whisper-cli -m <model> -l en -t 2 -np -nt -of /dev/null -f -`. The explicit output base enables transcript stdout with piped input on v1.9.6; no output-file format is enabled.

```sh
sudo systemctl restart robot-buddy
python3 scripts/pip-perf.py run \
  --variant baseline \
  --wake-mode neural \
  --ref-device 'plughw:CARD=USB_REF,DEV=0' --ref-channels 2 \
  --stim-device 'plughw:CARD=BENCH_SPK,DEV=0' \
  --turn-wav /tmp/bench-wavs/prompt-time.wav \
  --turn-wav /tmp/bench-wavs/prompt-weather.wav \
  --long-wav /tmp/bench-wavs/prompt-long-story.wav \
  --wake-wav /tmp/bench-wavs/wake-hey-pip.wav \
  --turns 20 --warmup 1 --barge 10 \
  --out /tmp/pip-perf-baseline.json
```

*(For transcript-only deployments without `BUDDY_LISTEN_WAKE_DIR`, pass `--wake-mode transcript` and omit `--barge`/`--long-wav`/`--wake-wav`: spoken barge-in is disabled by design in transcript mode, so `pip-perf.py` runs 0 barge-in trials, marks acoustic stop N/A, and enforces `0` echo interruptions.)*

### Variant B: `warm-whisper` (resident `whisper-server` inside the same 1 GiB cgroup)

Tests eliminating per-utterance `whisper-cli` spawn and ggml model-load latency **without changing any `robot-buddy` code or defaults**, while charging `whisper-server`'s resident memory honestly against the **exact same `robot-buddy.service` 1 GiB cgroup** (do **not** run `whisper-server` in a separate systemd unit, or its RSS will be hidden from `MemoryPeak`).

1. Build `whisper-server` from the same reviewed `whisper.cpp` release as `whisper-cli` and install to `/usr/local/bin/whisper-server`.
2. Install both benchmark-only scripts with mode `0755`:
   - `scripts/pip-perf-whisper-shim.sh` → `/usr/local/bin/pip-perf-whisper-shim.sh` (accepts the same CLI arguments under `env_clear()`, refuses non-loopback URLs, forwards stdin WAV to `http://127.0.0.1:8178/inference`, and cleans up its `PrivateTmp` temp file on exit).
   - `scripts/pip-perf-warm-whisper-exec.sh` → `/usr/local/bin/pip-perf-warm-whisper-exec.sh` (external `ExecStart=` wrapper avoiding inline systemd `$` expansion: starts `whisper-server` inside the same cgroup, waits for loopback readiness, runs `/usr/local/bin/robot-buddy-robot`, and terminates `whisper-server` cleanly on exit).
3. Add a temporary benchmark-only drop-in pointing `ExecStart=` at the wrapper and `BUDDY_LISTEN_WHISPER` at the shim:

```sh
sudo tee /etc/systemd/system/robot-buddy.service.d/90-bench-warm-whisper.conf >/dev/null <<'EOF'
[Service]
# Temporary benchmark-only drop-in: external wrapper keeps whisper-server and
# robot-buddy-robot inside the SAME 1 GiB cgroup so resident model RAM is
# counted in kernel memory.peak without inline systemd variable expansion.
NotifyAccess=all
Environment=BUDDY_LISTEN_WHISPER=/usr/local/bin/pip-perf-whisper-shim.sh
ExecStart=
ExecStart=/usr/local/bin/pip-perf-warm-whisper-exec.sh
EOF
sudo systemctl daemon-reload && sudo systemctl restart robot-buddy
```

4. Run the identical benchmark command with `--variant warm-whisper --out /tmp/pip-perf-warm-whisper.json`.
5. Remove `/etc/systemd/system/robot-buddy.service.d/90-bench-warm-whisper.conf`, run `sudo systemctl daemon-reload`, and restore the default service before Variant C.

### Variant C: `endpoint-shortening` (temporary scratch build; defaults unchanged)

Tests reducing VAD trailing silence (`END_FRAMES` in `apps/robot/src/listen/vad.rs`) from the default `40` frames (800 ms) to a benchmark candidate such as `25` frames (500 ms, saving 300 ms with ~0 extra RAM) in a **disposable scratch checkout/build** (never committed as default):

1. In a temporary copy of the repo (or reversible local patch), change `const END_FRAMES: usize = 40;` to `25` (500 ms), rebuild the Pi release binary (`cargo build -j2 --release --locked -p robot-buddy-robot --features pi-vision,onnx-wake`), and stage it temporarily.
2. Restart `robot-buddy.service` cleanly and run the identical benchmark command with `--variant endpoint-shortening --out /tmp/pip-perf-endpoint-shortening.json`.
3. In addition to latency, check natural-pause prompts (utterances containing a ~400–600 ms mid-sentence breath/pause) and record any premature clip splits in `--notes`.
4. Restore the unmodified baseline binary and restart `robot-buddy.service`.

### Side-by-side comparison & gate verdict

```sh
python3 scripts/pip-perf.py compare --strict \
  /tmp/pip-perf-baseline.json \
  /tmp/pip-perf-warm-whisper.json \
  /tmp/pip-perf-endpoint-shortening.json
```

Pass `--json` for machine-readable output. Under `--strict`, `compare` exits non-zero if any variant is unqualified (`allow_inactive`, `simulated`, missing camera/neck proof, unknown cgroup, or `< 20` turns / `< 10` barge-ins) or violates any provisional acceptance target (EoS→audible p95, wake END→stop p95, skipped 20 ms neck slot rate, `< 1 GiB` with 10% reserve, or safety faults/restarts/OOM).

Standalone offline analyzers are also available for individual WAVs or saved journal captures:

```sh
python3 scripts/pip-perf.py analyze-turn trial.wav [--prompt-end-ms 1400]
python3 scripts/pip-perf.py analyze-barge barge.wav [--wake-end-ms 2100]
python3 scripts/pip-perf.py parse-journal saved-journal.log
python3 scripts/pip-perf.py cgroup-snapshot --service robot-buddy.service
```

## 6. ARM64 recording-replay E2E fixture (no real hardware)

`tests/pip-replay-arm64.mjs` (backed by the zero-dependency C99 dual-PTY harness `scripts/pip-perf-pty-harness.c`) runs the **actual compiled `aarch64-unknown-linux-gnu` `robot-buddy-robot` binary in hardware mode (without `--simulate`)** against a real `robot-buddy-server` and real `rivet-engine` over TLS (`SSL_CERT_FILE`):

- **Audio recording replay**: Synthesizes 16 kHz mono S16_LE PCM clips and streams them through cue-driven `arecord` → `Alsa::frame` → `Vad::feed` → `wav(pcm)` → `whisper` stdin pipe → `Policy::heard` → `Control` → `Controller` → Rivet.
- **Face PTY assertions (`BUDDY_FACE_SERIAL`)**: Verifies raw 115200 `CLOCAL -HUPCL` termios reconfiguration, `{"ping":0}`, board `{"hello":"pip-face","v":1}` handling, `listening`/`thinking`/`speaking` LED transitions, live `face.express` override (`surprised` `0.9`), face impulse gesture (`ear_wiggle`), `asleep` (`1.0`), and clean `Offline` reset on `SIGTERM`.
- **Neck STS3215 PTY assertions (`BUDDY_SERVO_PORT` + `BUDDY_SERVO_CALIBRATION`)**: Verifies Linux 1 Mbaud (`B1000000`) termios, admission checks (`MODE=0`, `LOCK=1`, `LIMITS`, `PRESENT`), `TORQUE=1`, zero checksum errors, 50 Hz `SYNC WRITE` goal frames staying strictly inside calibrated `[min_steps, max_steps]`, pitch excursion during `surprised` + `nod`, asleep pose motion on `"Hey Pip, go to sleep"`, 50 Hz motion timing instrumentation parsed by `scripts/pip-perf.py parse-journal`, and confirmed `TORQUE=0` on all 3 servos during `SIGTERM` shutdown.
- **Acoustic barge-in**: Replays `"Hey Pip, stop talking"` mid-playback and asserts `aplay` is killed before EOF and the turn finishes as `turn.cancelled`.

> **What this fixture does not prove:** It uses synthesized PCM recordings, a mock model/TTS server, and virtual PTYs without physical camera frames or real `whisper.cpp` / ONNX / OpenCV inference. Physical hardware acceptance still requires the Pi 5 bench procedure in §4–§5, plus the dedicated simulated-Pip E2E and soak suite (`tests/pip-e2e/runner.mjs`).

## 7. Runnable verification commands

```sh
# 1. Motion 50 Hz timing instrumentation unit + loop tests
cargo test --locked -p robot-buddy-robot motion::

# 2. Offline acoustic analyzer, journal/cgroup parser, strict gate & warm-whisper shim tests
bun tests/pip-perf.mjs

# 3. ARM64 recording-replay E2E (builds or reuses target/pi-image/robot-buddy-robot)
bun tests/pip-replay-arm64.mjs
# Reuse an already-built ARM64 binary:
bun tests/pip-replay-arm64.mjs --no-build
```
