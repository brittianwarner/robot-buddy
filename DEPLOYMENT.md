# Deploy Robot Buddy on a Raspberry Pi 5

The Pi runs `robot-buddy-robot`. A separate host runs the Rust server and Rivet engine. Start with identity, network and playback; enable each peripheral independently after checking it. The Pi never needs the issuer key, Rivet admin token, OIDC secret or model-provider key. Models, voices, firmware and the private reference project are **not** part of the Pi binary or this repository.

## 1. Bring up the backend and issue one device identity

On the trusted provisioning machine, from the repository root, with the [pinned Rust toolchain](rust-toolchain.toml):

```sh
umask 077
# First installation of the backend only. Keep the issuer key offline afterwards.
cargo run --locked -p robot-buddy-server -- init secrets
cp .env.example .env
# Each new robot: use a different output file, never a copy from another SD card.
cargo run --locked -p robot-buddy-server -- provision secrets/issuer.key secrets/pip-01.identity.json
```

On the **server host**, populate `.env` using the three values in `secrets/secrets.env` and [.env.example](.env.example). Set `BUDDY_PUBLIC_URL` and `BUDDY_PUBLIC_RIVET_URL` to two HTTPS origins reachable from the Pi. Set `AI_PROVIDER`, `AI_API_KEY`, `AI_MODEL`; for cloud speech set `TTS_MODEL=elevenlabs/eleven-v4-turbo` with an OpenRouter key. For claiming, register `<BUDDY_PUBLIC_URL>/auth/callback` at the OIDC provider and set `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET`.

```sh
chmod 0600 .env
docker compose up --build -d
docker compose ps
curl --fail http://127.0.0.1:8080/ready
```

Route the registration origin to `127.0.0.1:8080`, and the Rivet origin (including WebSocket upgrades) to `127.0.0.1:6420` through your TLS proxy. Do not log claim URLs, request credentials or query strings. [compose.yaml](compose.yaml) mounts a named volume at `/data`. One running engine per volume; stop it for a consistent backup and keep the matching secrets. Use the [backend guide](docs/architecture/backend.md) for restore or S3 setup. The local Compose lab and `--simulate` are development paths, not the hardware deployment.

## 2. Build the Linux ARM64 binary

Use **64-bit Raspberry Pi OS** (`uname -m` → `aarch64`, `dpkg --print-architecture` → `arm64`). Existing **Bookworm** Pis can run all peripherals without an OS upgrade: follow the native Bookworm build below. For **Trixie**, use its packaged OpenCV or the ARM64 Buildx recipe. The target must supply the same OpenCV and glibc ABI the binary was built against; do **not** copy a Trixie binary onto Bookworm. Models are installed later on the device.

| Where | Build from the repository root | Artifact |
|---|---|---|
| Pi 5, no camera | `cargo build --release --locked -p robot-buddy-robot` | `target/release/robot-buddy-robot` |
| Bookworm Pi 5, full | Build the private-prefix OpenCV and native robot below | `target/release/robot-buddy-robot` |
| Trixie Pi 5, full | Install `build-essential cmake clang libclang-dev pkg-config libopencv-dev`, then `cargo build -j2 --release --locked -p robot-buddy-robot --features pi-vision,onnx-wake` | `target/release/robot-buddy-robot` |
| Mac or Linux with Docker Buildx, with camera | Command below | `target/pi-image/robot-buddy-robot` |
| Linux cross-compiler, no camera | GNU toolchain commands below | `target/aarch64-unknown-linux-gnu/release/robot-buddy-robot` |

### Existing Bookworm Pi: native full build

Bookworm's packaged OpenCV 4.6 predates the recommended 2023 YuNet. Build **OpenCV 4.10** beside the OS libraries rather than replacing the distro's camera stack. From a regular account on an ARM64 Bookworm Pi (or an equivalent ARM64 Bookworm build host):

```sh
sudo apt-get install build-essential git cmake ninja-build clang libclang-dev pkg-config libjpeg-dev zlib1g-dev
# Use a fresh directory on a drive with space to compile; -j2 limits peak memory.
git clone --depth 1 --branch 4.10.0 https://github.com/opencv/opencv.git opencv-4.10.0
test "$(git -C opencv-4.10.0 rev-parse HEAD)" = 71d3237a093b60a27601c20e9ee6c3e52154e8b1 || exit 1
CV=/opt/robot-buddy/opencv-4.10
cmake -S opencv-4.10.0 -B opencv-pi-build -G Ninja \
  -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$CV" -DOPENCV_LIB_INSTALL_PATH=lib \
  -DBUILD_LIST=core,imgproc,imgcodecs,dnn,objdetect -DBUILD_SHARED_LIBS=ON \
  -DOPENCV_GENERATE_PKGCONFIG=ON -DWITH_JPEG=ON -DWITH_OPENCL=OFF \
  -DBUILD_TESTS=OFF -DBUILD_PERF_TESTS=OFF -DBUILD_EXAMPLES=OFF
cmake --build opencv-pi-build -j2
sudo cmake --install opencv-pi-build
# Register only this root-owned library directory so systemd can resolve the new SONAMEs.
printf '%s/lib\n' "$CV" | sudo tee /etc/ld.so.conf.d/robot-buddy-opencv.conf >/dev/null
sudo ldconfig
export PKG_CONFIG_PATH="$CV/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
test "$(pkg-config --modversion opencv4)" = 4.10.0 || exit 1
test "$(pkg-config --variable=prefix opencv4)" = "$CV" || exit 1
# From the robot-buddy repo, using its pinned Rust toolchain:
cargo build -j2 --release --locked -p robot-buddy-robot --features pi-vision,onnx-wake
ldd target/release/robot-buddy-robot | grep opencv  # paths under /opt/robot-buddy/opencv-4.10/lib
```

If building for another **matching Bookworm Pi**, deploy the contents of that OpenCV prefix at the same absolute path, including SONAME symlinks (`tar`/`rsync -a`), then install the same linker entry, run `ldconfig` and inspect `ldd` **on the target**. This path does not require `libopencv-dev` 4.6 or a copied Trixie system library. Install the ONNX Runtime for optional neural wake separately (§4).

### Trixie from another workstation, or a base build without vision

The [Pi build image](infra/pi-build.Dockerfile) compiles inside an ARM64 Trixie userspace, using native execution on ARM64 Docker or QEMU on a configured x86 Buildx builder. Thus libclang, the OpenCV headers and the C++ linker see **target** packages. Installing macOS OpenCV, or setting only Cargo's target triple, cannot achieve that. No Docker daemon is needed on the Pi.

```sh
docker buildx inspect --bootstrap             # Platforms must include linux/arm64
docker buildx build --platform linux/arm64 -f infra/pi-build.Dockerfile \
  --output type=local,dest=target/pi-image .
file target/pi-image/robot-buddy-robot        # ELF 64-bit ... ARM aarch64, not Mach-O
# Optional: execute the robot module tests inside the same Linux ARM64 userspace.
docker buildx build --platform linux/arm64 --target test -f infra/pi-build.Dockerfile .
```

For the client **without vision**, the GNU setup used by [CI](.github/workflows/ci.yml) also supports release linking on a Linux build host. Use a build distribution whose glibc is no newer than the Pi's:

```sh
sudo apt-get install gcc-aarch64-linux-gnu libc6-dev-arm64-cross
rustup target add aarch64-unknown-linux-gnu
export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
cargo build --locked --release -p robot-buddy-robot --target aarch64-unknown-linux-gnu
```

[.cargo/config.toml](.cargo/config.toml) selects Cortex-A76 for this target. Don't use this image for a 32-bit or older Pi. For release checks use `bun install --frozen-lockfile && bun run check:all` on the build workstation; mocks do not exercise the physical devices.

## 3. Prepare the Pi and install the service

Install Raspberry Pi OS Lite and enable SSH over a trusted provisioning network. Use a Wi-Fi interface with AP mode and NetworkManager. Keep initial access over Ethernet if possible. In the Pi commands below run administrative steps as root. Stop any pre-existing `pip.service` **if present** (`systemctl disable --now pip.service`); two processes must not own the mic, serial ports or camera.

```sh
apt-get update
apt-get install network-manager polkitd alsa-utils socat ca-certificates
systemctl enable --now NetworkManager
# Camera profile only. On Bookworm use the OpenCV prefix built above.
apt-get install rpicam-apps
# Trixie packaged/Buildx vision build ONLY: installs matching runtime libraries.
# apt-get install libopencv-dev
```

On a standard MAX98357A I²S installation, enable `dtoverlay=max98357a` in `/boot/firmware/config.txt` and reboot. Use the overlay appropriate to your wiring, not two conflicting sound overlays. For OV5647 first try the default camera auto-detection; if necessary specify `dtoverlay=ov5647,cam0` with auto-detect disabled and the correct CSI socket. Use separately rated servo power, shared ground and a Pi supply rated for its attached USB load. Software can't confirm torque-off after lost power, a broken cable or a killed process; keep the servo supply switch reachable.

Stage **only** the binary, this robot's identity, and the files under `infra/` on the Pi over SSH. Example from the provisioning machine (adjust user, hostname and binary):

```sh
ssh pi@pip.local 'umask 077; mkdir -p ~/buddy-install/infra'
scp target/pi-image/robot-buddy-robot secrets/pip-01.identity.json pi@pip.local:buddy-install/
scp infra/{robot-buddy.service,robot-buddy-nm.rules,robot-buddy-pip.conf,device.env.example} pi@pip.local:buddy-install/infra/
```

On the Pi, change into that staging directory, then run as root. On **first install**:

```sh
getent group pip-control >/dev/null || groupadd --system pip-control
id robot-buddy >/dev/null 2>&1 || useradd --system --user-group --home-dir /var/lib/robot-buddy \
  --shell /usr/sbin/nologin robot-buddy
usermod -aG audio,dialout,pip-control robot-buddy
install -d -m 0750 -o root -g robot-buddy /etc/robot-buddy
install -d -m 0700 -o robot-buddy -g robot-buddy /var/lib/robot-buddy
install -m 0755 robot-buddy-robot /usr/local/bin/
ldd /usr/local/bin/robot-buddy-robot            # resolve every entry (none 'not found')
install -m 0600 -o robot-buddy -g robot-buddy pip-01.identity.json /etc/robot-buddy/device.identity.json
rm pip-01.identity.json                       # remove the extra staged private key
install -m 0600 -o root -g root infra/device.env.example /etc/robot-buddy/device.env
install -m 0644 infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/50-robot-buddy-nm.rules
install -m 0644 infra/robot-buddy.service /etc/systemd/system/
```

Set `BUDDY_SERVER_URL` in `/etc/robot-buddy/device.env` to the registration HTTPS origin. Set `BUDDY_WIFI_INTERFACE` if not `wlan0`. For playback set `BUDDY_SPEECH=true` and `BUDDY_ALSA_DEVICE` to the actual named `plughw:CARD=…,DEV=0` from `aplay -L`; do not rely on a logged-in user's PipeWire. Keep event detail compact. Leave the optional features in §4 unset initially.

```sh
# Same user and state path as the service. Print/store the card privately.
runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot setup-card
systemd-analyze verify /etc/systemd/system/robot-buddy.service
systemctl daemon-reload
systemctl enable --now robot-buddy
systemctl status --no-pager robot-buddy
stat -c '%U:%G %a' /run/robot-buddy/control.sock   # robot-buddy:pip-control 660
```

When no working network is available, join the printed per-device setup SSID with its password, open `http://10.42.0.1` and choose the home network. Do not enable a wait-online dependency: this unit provides setup when offline. Once connected, open the claim URL on the optional loopback panel (`127.0.0.1:8090`) or obtain it locally with `runuser -u robot-buddy -- robot-buddy-robot claim-url`. Complete OIDC sign-in and confirm the **matching six digits** on the robot's panel or through speech. Treat setup cards, claim URLs and device journals as private. Preserve the printed card even if the eyes display setup QR codes.

## 4. Enable hardware one subsystem at a time

Edit `/etc/robot-buddy/device.env`, then `systemctl restart robot-buddy` after each change. Inspect `journalctl -u robot-buddy -b` locally. For camera or local ML, install the [optional budget/group drop-in](infra/robot-buddy-pip.conf) first:

```sh
install -D -m 0644 infra/robot-buddy-pip.conf /etc/systemd/system/robot-buddy.service.d/20-pip.conf
systemctl daemon-reload
systemctl restart robot-buddy
systemctl show robot-buddy -p SupplementaryGroups -p MemoryMax -p TasksMax
```

The base unit has a 256 MiB budget and 128 tasks. The drop-in adds `video` and sets initial budgets of 1 GiB and 256 tasks for the entire cgroup (including children). These are starting ceilings, not measured requirements. Keep `MemoryDenyWriteExecute=yes`, the watchdog, unprivileged user and filesystem restrictions. On customized images inspect `ls -l /dev/{video*,media*,dma_heap/*}`; add only the necessary existing device group, not root or world-write permissions.

### Face and neck

* **Face:** use the existing ESP32-S3 N16R8 firmware and its native **USB**, not UART. If reflashing is needed, use the authorized firmware checkout's `firmware/` PlatformIO project: `pio run`, `pio run -t upload`; unplug the serial monitor before starting the daemon. Pick its distinct path from `ls -l /dev/serial/by-id/` and set `BUDDY_FACE_SERIAL` to that path. Verify both eyes, ring, ear movement and firmware heartbeat; unplug/replug the face and check recovery.
* **Neck:** the CH343/STS3215 adapter has a **different** serial path. If an adapter lacks a unique ID, use a stable USB topology path under `/dev/serial/by-path/`. With the service stopped and head supported, assign each servo a distinct ID in position mode using the vendor tool (one servo connected at a time). Record its straight-ahead raw position, inversion and conservative cable-safe range on the assembled robot. Do not copy calibration between robots.

Create a root-owned `0640 root:robot-buddy` JSON file `/etc/robot-buddy/neck.json`. It has three required objects: `yaw`, `pitch`, `roll`; each contains `id`, `center`, `min_steps`, `max_steps`, `invert`, `max_speed_deg_s` and `accel`. Example **shape of one joint only**, not a calibration for your robot: `"yaw":{"id":1,"center":2048,"min_steps":2000,"max_steps":2100,"invert":false,"max_speed_deg_s":10,"accel":5}`. All counts are in 0–4095; min < center < max. Confirm all IDs and ranges before writing EEPROM. Never widen limits to make a failed verification pass.

```sh
systemctl stop robot-buddy
PORT=/dev/serial/by-id/YOUR_SERVO_ADAPTER
CAL=/etc/robot-buddy/neck.json
runuser -u robot-buddy -- robot-buddy-robot servo ping "$PORT" "$CAL"
runuser -u robot-buddy -- robot-buddy-robot servo read "$PORT" "$CAL"
# Checks position mode, EEPROM limits/lock and joint position; leaves torque off.
runuser -u robot-buddy -- robot-buddy-robot servo verify "$PORT" "$CAL"
# ONLY for deliberate commissioning after measuring physical limits:
# runuser -u robot-buddy -- robot-buddy-robot servo provision "$PORT" "$CAL" --write-eeprom
# Verify again afterwards. Only then exercise center under supervision:
# runuser -u robot-buddy -- robot-buddy-robot servo center "$PORT" "$CAL"
```

After verification set `BUDDY_SERVO_PORT` and `BUDDY_SERVO_CALIBRATION` to those paths. Start the service, test a small pose, then `systemctl stop robot-buddy` and confirm the head can relax. Remove both settings to run without the neck. Consult the firmware's own assembly documentation for wiring and ear calibration.

### Microphone, transcription and speaker

The integrated path uses local `arecord` → voice activity detection → `whisper-cli` → transcript wake matching. It does **not** need Python or a private wake model. Use `arecord -l`/`arecord -L` for the reSpeaker and `aplay -L` for the separate amplifier. Verify 16 kHz S16_LE capture with the correct USB channel count as `robot-buddy`, with the service stopped, e.g. `runuser -u robot-buddy -- arecord -D 'plughw:CARD=YOUR_MIC,DEV=0' -r16000 -fS16_LE -c1 -d2 -t raw /dev/null`.

Install a reviewed [whisper.cpp](https://github.com/ggml-org/whisper.cpp) release on the Pi (for example, configure its release source with `cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF`, `cmake --build build -j2 --target whisper-cli`, then `install -m755 build/bin/whisper-cli /usr/local/bin/`). Select a local, license-reviewed English ggml model, e.g. `ggml-tiny.en.bin`. For every model in this guide, verify the pinned revision and publisher checksum **before installation**; keep the license/model card with the device inventory. The service does not download weights on first boot. Install models read-only by the service under `/opt/robot-buddy/models`, outside `/home`.

```ini
BUDDY_LISTEN_MODEL=/opt/robot-buddy/models/ggml-tiny.en.bin
BUDDY_LISTEN_WHISPER=/usr/local/bin/whisper-cli
BUDDY_LISTEN_DEVICE=plughw:CARD=YOUR_MIC,DEV=0
BUDDY_LISTEN_CHANNELS=1
BUDDY_LISTEN_CHANNEL=0
```

`CHANNELS` is the captured interleaved channel count, `CHANNEL` the zero-based beam index. Say “Hey Pip, what time is it?”; check a reply, a short follow-up without a wake word, and a wake while speaking. If playback triggers the mic, check beam/amp routing and set `BUDDY_LISTEN_BARGE_IN=0` until tested. The external socket still works without this listener: put only the external STT account in `pip-control` (not `robot-buddy`). Test as that account: `printf '{"wake":true}\n' | socat - UNIX-CONNECT:/run/robot-buddy/control.sock`. Do not enable two capture processes on the same microphone.

**Optional neural wake:** the full image includes the `onnx-wake` Cargo feature. Install a compatible AArch64 Linux CPU build of [ONNX Runtime](https://github.com/microsoft/onnxruntime/releases) (1.20 or newer compatible ABI); point `ORT_DYLIB_PATH` at its actual `libonnxruntime.so`, not a directory. Use a local directory containing the authorized `hey_pip.onnx` classifier plus the matching openWakeWord `melspectrogram.onnx` and `embedding_model.onnx` frontend. Check each model's terms, especially for commercial use; no weights are supplied. Set e.g. `BUDDY_LISTEN_WAKE_DIR=/opt/robot-buddy/models/wake` alongside `BUDDY_LISTEN_MODEL`. This changes wake detection, not speech transcription. Validate quiet, noisy, positive and negative recorded clips against the reference before enabling it unattended; remove `BUDDY_LISTEN_WAKE_DIR` to return to transcript wake.

For a repeatable wake check on a private test machine with the matching native ONNX Runtime installed, set `ORT_DYLIB_PATH`, `BUDDY_LISTEN_WAKE_DIR`, `BUDDY_WAKE_TEST_PCM` (16 kHz mono S16_LE, 1280 samples per hop) and `BUDDY_WAKE_TEST_SCORES` (one numeric reference score per hop). Include both classes after the four-second warmup. Run `cargo test --locked -p robot-buddy-robot --features onnx-wake compare_reference_scores -- --ignored`. No clips, reference scores or models should be added to the repo.

**Optional offline voice:** install [Piper](https://github.com/OHF-Voice/piper1-gpl) separately (e.g. in a Pi-compatible, root-owned venv under `/opt/robot-buddy/piper`, with a reviewed pinned `piper-tts` version). It must accept `--model`, `--output-raw`, `--length-scale` and emit signed 16-bit mono PCM on stdout. Install the chosen `.onnx` **and its matching `.onnx.json`**; the latter gives the sample rate. Use the voice's own model license. No Piper or espeak code is linked into the client.

```ini
BUDDY_PIPER_BIN=/opt/robot-buddy/piper/bin/piper
BUDDY_PIPER_MODEL=/opt/robot-buddy/models/voice.onnx
```

This announces a fixed offline status locally, not model replies. Ordinary speech still streams from the server. Omit `BUDDY_PIPER_MODEL` to disable it. On the bench test loss and recovery of the backend and confirm mute and interruption silence the device.

### Camera, gallery and operator controls

With the service stopped check `rpicam-hello --list-cameras`. Then check `rpicam-vid --nopreview --frames 1 -t 0 --width 384 --height 288 --framerate 15 --codec yuv420 -o - | wc -c`: a full packed frame is **165888 bytes**. Use the `pi-vision` binary; matching occurs in-process with CPU OpenCV. Install the authorized YuNet 2023mar ONNX model for detection. Only for opt-in recognition also install SFace 2021dec. The [vision guide](apps/robot/src/vision/README.md) gives pinned upstream revisions, checksums and a private recognition fixture test. Fetch the actual LFS object, not its text pointer. No external vision worker is required.

```ini
BUDDY_VISION=1
BUDDY_VISION_YUNET=/opt/robot-buddy/models/face_detection_yunet_2023mar.onnx
# Optional; leaving SFace unset keeps detection without identification.
#BUDDY_VISION_SFACE=/opt/robot-buddy/models/face_recognition_sface_2021dec.onnx
#BUDDY_VISION_GALLERY=/var/lib/robot-buddy/faces
#BUDDY_VISION_ROTATE_180=1
```

The gallery belongs in the service's `0700` state directory. Use the operator panel to enroll or forget local embeddings. Frame data and recognition are not a cloud identity or authority to confirm claims. To enable the panel, put a unique `BUDDY_OPERATOR_TOKEN` (32–128 printable non-space ASCII characters, e.g. `openssl rand -hex 32`) into the root-only env file, **not** an SSH command or a URL. Restart the service. Once connected and claimed:

```sh
# From the operator's computer. Keep the same loopback host and port at both ends.
ssh -N -L 127.0.0.1:8091:127.0.0.1:8091 pi@pip.local
```

Open `http://127.0.0.1:8091/` locally; Basic auth username `operator`, password the token. Check preview and live events, then exercise a small bounded manual pose and release it; test enrollment with only one consenting person in view and test forgetting. This is separate from the unprivileged onboarding screen on `8090`. Do not publish either port to the LAN or your TLS reverse proxy. Remove the token and restart to disable the operator panel.

## 5. Acceptance, upgrades and recovery

Before leaving a robot unattended, record the binary revision/checksum, OS and model versions, calibration file revision and outcomes on **that robot**:

**What was verified before bench commissioning:** both the Bookworm private-prefix OpenCV 4.10 build and the Trixie packaged OpenCV build linked the full `aarch64-unknown-linux-gnu` release (`pi-vision,onnx-wake`) inside ARM64 Debian containers. The Trixie binary ran `setup-card` unprivileged, and its full-feature robot test suite passed (189 passed; the two tests requiring private recordings or labeled images were skipped). These checks do not establish motor direction, camera accuracy, wake behavior, timing, or memory headroom on a physical Pi.

| Exercise | Observable result |
|---|---|
| Cold boot, reboot with known Wi-Fi, then loss and return of the uplink | Same bot and ownership; setup remains available with no network, no stale speech after reconnect. |
| Face heartbeat, disconnect/replug, invalid/missing helper or model | Diagnose locally in the journal; issue is visible, credentials and conversational text are not diagnostic material. |
| Local wake in quiet/noisy conditions, mute, ten minutes of played speech | Measure misses, false triggers and actual stop latency; do not assume echo cancellation. |
| Slow manual pose and release; stop/restart service with supported head | No unexpected jump or overtravel. Confirm physical torque release; if unconfirmed, cut servo power and inspect before restart. |
| Camera present/absent, enrollment and forgetting, closed preview | Recognition is optional; no stale target controls access. Panel rejects unclaimed/unauthenticated access. |
| Vision + speech + motion together | Measure peak memory, task count, CPU temperature and underruns; verify watchdog, no OOM and acceptable latency. |

Use `systemctl show robot-buddy -p NRestarts -p MemoryCurrent -p MemoryPeak -p TasksCurrent -p Result`, `systemd-cgtop`, `journalctl -u robot-buddy -b`, and `vcgencmd get_throttled`. For camera/ALSA permissions inspect groups on actual nodes; for absent ML dependencies check `ldd`, `pkg-config --modversion opencv4` (if development packages installed), executable paths and model file permissions. A `403` after valid operator credentials requires a connected, claimed bot and the expected origin; `401` means credentials are missing or wrong. The operator panel does not bind a non-loopback address.

**Upgrade:** save the current working binary, root-only env file, unit/drop-ins and calibration. Stop the service; replace the binary and any changed unit/rules, verify the new binary's dependencies, run `systemctl daemon-reload` and restart. Keep `/etc/robot-buddy/device.identity.json`, `/var/lib/robot-buddy/` (cache, setup card, gallery) and the server's volume/secrets. Do not re-run `init`, `provision` or overwrite the env file during an upgrade. To roll back, stop the unit, restore the saved binary and matching configuration/calibration, reload and restart. If EEPROM limits changed, reconcile them **with power to the motors under operator control** and verify before rearming. A device rollback does not roll back server state. Removing optional hardware flags is the quickest way to isolate a peripheral failure without changing identity.
