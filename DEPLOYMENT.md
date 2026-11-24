# Deploy Robot Buddy on a Raspberry Pi 5

The Pi runs `robot-buddy-robot`. A separate host runs the Rust server and Rivet engine. Start with identity, network and playback; enable each peripheral independently after checking it. The Pi never needs the issuer key, Rivet admin token, OIDC secret or model-provider key. See [NOTICE.md](NOTICE.md) for provenance and licensing boundaries: the neck choreography/expression poses and local voice-coloring adaptations were developed in collaboration with ToschLogic (not as an MIT relicense or redistribution grant), while models, voices, firmware sources, recordings and the private reference tree are **not** bundled in the Pi binary or this repository.

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

On the **server host**, populate `.env` using the three values in `secrets/secrets.env` and [.env.example](.env.example). Set `BUDDY_PUBLIC_URL` and `BUDDY_PUBLIC_RIVET_URL` to two HTTPS origins reachable from the Pi. Set `AI_PROVIDER`, `AI_API_KEY`, `AI_MODEL`; for cloud speech set `TTS_MODEL=elevenlabs/eleven-v4-turbo` with an OpenRouter key.

**OIDC requirements for claiming:** register `<BUDDY_PUBLIC_URL>/auth/callback` at your provider and set `OIDC_ISSUER`, `OIDC_CLIENT_ID`, and (for confidential clients) `OIDC_CLIENT_SECRET` (omit it for public PKCE clients; when set, it is sent via `client_secret_post`).
- **Discovery availability at startup:** when `OIDC_ISSUER` is set, the server fetches `<OIDC_ISSUER>/.well-known/openid-configuration` once over HTTPS **at boot** with HTTP redirects disabled (20 s timeout). Startup fails if the discovery endpoint is unreachable, redirects, or returns an `issuer` string that does not match `OIDC_ISSUER` verbatim (including any trailing slash). Leave `OIDC_ISSUER` unset to boot without browser claiming.
- **Provider capabilities:** the discovery document must publish HTTPS `authorization_endpoint`, `token_endpoint`, `userinfo_endpoint`, and `jwks_uri` (all answering without HTTP redirects). The client must support Authorization Code + PKCE (`S256`), `scope=openid profile`, `RS256` or `ES256` ID tokens carrying `kid` and `nonce`, and a `userinfo_endpoint` returning the same `sub` as the ID token.
- **Browser redirect & error behavior:** multi-origin HTTPS redirects during sign-in (for example, federated SSO or a separate login domain) are permitted by `/claim`'s `form-action 'self' https:` policy; plaintext `http:` hops are not. If the user cancels or the IdP returns `?error=...&state=...`, `/auth/callback` verifies the login cookie and `state`, spends the transaction, clears `buddy_login`, and renders a static retry page without echoing query parameters.

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
# Camera profile only (`rpicam-apps-lite` avoids desktop preview dependencies on Lite; `rpicam-apps` also works).
apt-get install rpicam-apps-lite
# Trixie prebuilt/Buildx vision binary runtime libraries ONLY (on Bookworm use the /opt/robot-buddy/opencv-4.10 prefix built above):
# apt-get install libopencv-core410 libopencv-imgproc410 libopencv-imgcodecs410 libopencv-dnn410 libopencv-objdetect410
```

**Power supply and `/boot/firmware/config.txt`:** Pip's reference power topology feeds a 12 V 5 A supply into three branches with a **shared ground** across every rail: (1) Buck A (12 V → 5 V 5 A plain USB-C buck, **not** USB-PD) dedicated to the Pi 5 alone (must actually hold 5 A at ~5.1 V); (2) Buck B (12 V → 5 V 5 A) for the MG90S ear servos and WS2812B chest ring (never join the Pi USB 5 V rail to Buck B); and (3) 12 V direct to the STS3215 serial bus servo adapter. Keep the 12 V servo power switch reachable: software cannot confirm torque-off after lost power, a broken cable or a killed process.

Because a plain 5 V USB-C buck does not negotiate USB-PD 5 A, the Pi 5 defaults to capping total USB downstream current at 600 mA across the ESP32-S3, reSpeaker, and servo adapter. Edit `/boot/firmware/config.txt` and reboot:

```ini
# Set ONLY when powering the Pi 5 from a plain (non-USB-PD) supply genuinely rated
# for 5 V / 5 A (such as Buck A above). Omit on an official 27 W USB-PD 5 A supply
# (which negotiates 5 A over PD), and never set on an under-rated 3 A supply.
usb_max_current_enable=1

# MAX98357A I2S amp (BCLK GPIO18, LRCLK GPIO19, DIN GPIO21): disable onboard audio
# so ALSA does not conflict with the I2S overlay. Use only the overlay for your wiring.
dtparam=audio=off
dtoverlay=max98357a

# OV5647 CSI camera: default camera_auto_detect=1 usually finds it. If
# `rpicam-hello --list-cameras` shows nothing, disable auto-detect and select the wired CSI port:
# camera_auto_detect=0
# dtoverlay=ov5647,cam0
```

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
# Priority 40 runs before Debian/RPi OS 49-polkit-pkla-compat.rules and 50-* defaults.
# Inspect /etc/polkit-1/rules.d/ and /etc/polkit-1/localauthority/ first so this does not
# overturn a stricter site NetworkManager policy; remove any legacy 50-robot-buddy-nm.rules.
install -m 0644 infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
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

The base unit has a 256 MiB budget and 128 tasks. The drop-in adds `video` and sets initial budgets of 1 GiB and 256 tasks for the entire cgroup (including children). These are starting ceilings, not measured requirements. Keep `MemoryDenyWriteExecute=yes`, the watchdog, unprivileged user, `SystemCallFilter` directives and filesystem restrictions. For camera hardware, also add `robot-buddy` to `video` (`usermod -aG video robot-buddy`) so unprivileged `runuser -u robot-buddy` camera checks have the same group membership as the unit. On customized images inspect `ls -l /dev/{video*,media*,v4l-subdev*,dma_heap/*}` (and `/dev/dri/renderD*` if present); add only the necessary existing device group(s) (typically `video`; add `render` only if a required node is `render`-owned) to both the user and `20-pip.conf`, never root or world-write permissions.

### Face and neck

* **Face:** use the existing ESP32-S3 DevKitC-1 N16R8 firmware and its native **USB** port, not UART. If reflashing is needed, **stop the service first** (`systemctl stop robot-buddy`) so the daemon does not hold the CDC-ACM port open, then run `pio run` and `pio run -t upload` from the authorized firmware checkout's `firmware/` PlatformIO project. If automatic upload fails to enter download mode, enter the ROM bootloader manually (**hold `BOOT`, tap `RST`, release `BOOT`**), run `pio run -t upload` again, and tap `RST` afterwards if needed. Close `pio device monitor` before starting `robot-buddy` again. Pick its distinct path from `ls -l /dev/serial/by-id/` and set `BUDDY_FACE_SERIAL` to that path. Verify both eyes, ring, ear movement and firmware heartbeat; unplug/replug the face and check recovery.
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

The integrated path uses local `arecord` → voice activity detection → `whisper-cli` → transcript wake matching. It does **not** need Python or a private wake model. Use `arecord -l`/`arecord -L` for the reSpeaker and `aplay -L` for the separate amplifier. Pip's USB reSpeaker (reSpeaker Lite 2-mic / XVF3800) presents a **2-channel** 16 kHz S16_LE capture stream: open it at its native 2-channel layout (`-c2`, `BUDDY_LISTEN_CHANNELS=2`) and select the processed beam (`BUDDY_LISTEN_CHANNEL=0`, or `1` if your firmware exposes the ASR beam on channel 1) so `plughw:` does not downmix both beams together. Verify capture as `robot-buddy`, with the service stopped, e.g. `runuser -u robot-buddy -- arecord -D 'plughw:CARD=YOUR_MIC,DEV=0' -r16000 -fS16_LE -c2 -d2 -t raw /dev/null` (adjust `-c` and `BUDDY_LISTEN_CHANNELS` only if your microphone's native channel count differs).

Install a reviewed [whisper.cpp](https://github.com/ggml-org/whisper.cpp) release on the Pi (for example, configure its release source with `cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF`, `cmake --build build -j2 --target whisper-cli`, then `install -m755 build/bin/whisper-cli /usr/local/bin/`). Select a local, license-reviewed English ggml model, e.g. `ggml-tiny.en.bin`. For every model in this guide, verify the pinned revision and publisher checksum **before installation**; keep the license/model card with the device inventory. The service does not download weights on first boot. Install models read-only by the service under `/opt/robot-buddy/models`, outside `/home`.

```ini
BUDDY_LISTEN_MODEL=/opt/robot-buddy/models/ggml-tiny.en.bin
BUDDY_LISTEN_WHISPER=/usr/local/bin/whisper-cli
BUDDY_LISTEN_DEVICE=plughw:CARD=YOUR_MIC,DEV=0
BUDDY_LISTEN_CHANNELS=2
BUDDY_LISTEN_CHANNEL=0
```

`CHANNELS` is the captured interleaved channel count, `CHANNEL` the zero-based beam index. With transcript wake, say “Hey Pip, what time is it?” and test a follow-up after the reply. **Spoken barge-in requires neural wake:** transcript mode does not transcribe playback echo to search for “Hey Pip”, even when `BUDDY_LISTEN_BARGE_IN=1`. With neural wake enabled, test an interruption during speech; set `BUDDY_LISTEN_BARGE_IN=0` if the beam/amp routing does not reject echo. The external socket still works without this listener: put only the external STT account in `pip-control` (not `robot-buddy`). Test as that account: `printf '{"wake":true}\n' | socat - UNIX-CONNECT:/run/robot-buddy/control.sock`. Do not enable two capture processes on the same microphone.

**Optional neural wake:** the full image includes the `onnx-wake` Cargo feature. Install a compatible AArch64 Linux CPU build of [ONNX Runtime](https://github.com/microsoft/onnxruntime/releases) (1.20 or newer compatible ABI; verify the publisher checksum); point `ORT_DYLIB_PATH` at its actual `libonnxruntime.so`, not a directory. Use a local directory containing the authorized private `hey_pip.onnx` classifier plus the matching public openWakeWord `melspectrogram.onnx` and `embedding_model.onnx` frontend (`v0.5.1`). Check each model's terms, especially for commercial use; no weights are supplied and no hash is published here for the private `hey_pip.onnx` asset (verify it against the authorized owner's provisioning record). Set e.g. `BUDDY_LISTEN_WAKE_DIR=/opt/robot-buddy/models/wake` alongside `BUDDY_LISTEN_MODEL`. This changes wake detection, not speech transcription. Validate quiet, noisy, positive and negative recorded clips against the reference before enabling it unattended; remove `BUDDY_LISTEN_WAKE_DIR` to return to transcript wake.

#### Verified public model pins

Verify every public model's SHA-256 (`sha256sum`) before copying it to `/opt/robot-buddy/models` (fetch real LFS bytes, not text pointers). Private assets (`hey_pip.onnx`) and operator-chosen Piper voices are omitted from this table—verify those against their own owner/publisher checksums rather than fabricating hashes:

| Public model | Upstream pin | Size (bytes) | SHA-256 |
|---|---|---:|---|
| `ggml-tiny.en.bin` | `ggerganov/whisper.cpp` (`5359861c739e955e79d9a303bcbc70fb988958b1`) | 77,704,715 | `921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f` |
| `melspectrogram.onnx` | `dscripka/openWakeWord` release `v0.5.1` | 1,087,958 | `ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f` |
| `embedding_model.onnx` | `dscripka/openWakeWord` release `v0.5.1` | 1,326,578 | `70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f` |
| `face_detection_yunet_2023mar.onnx` | `opencv/opencv_zoo` (`48b10fa505aeb51fbd127fc60a5c660040e823b7`) | 232,589 | `8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4` |
| `face_recognition_sface_2021dec.onnx` | `opencv/opencv_zoo` (`48b10fa505aeb51fbd127fc60a5c660040e823b7`) | 38,696,353 | `0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79` |

For a repeatable wake check on a private test machine with the matching native ONNX Runtime installed, set `ORT_DYLIB_PATH`, `BUDDY_LISTEN_WAKE_DIR`, `BUDDY_WAKE_TEST_PCM` (16 kHz mono S16_LE, 1280 samples per hop) and `BUDDY_WAKE_TEST_SCORES` (one numeric reference score per hop). Include both classes after the four-second warmup. Run `cargo test --locked -p robot-buddy-robot --features onnx-wake compare_reference_scores -- --ignored`. No clips, reference scores or models should be added to the repo.

**Optional offline voice:** install [Piper](https://github.com/OHF-Voice/piper1-gpl) separately (e.g. in a Pi-compatible, root-owned venv under `/opt/robot-buddy/piper`, with a reviewed pinned `piper-tts` version). It must accept `--model`, `--output-raw`, `--length-scale` and emit signed 16-bit mono PCM on stdout. Install the chosen `.onnx` **and its matching `.onnx.json`**; the latter gives the sample rate. Use the voice's own model license. No Piper or espeak code is linked into the client.

```ini
BUDDY_PIPER_BIN=/opt/robot-buddy/piper/bin/piper
BUDDY_PIPER_MODEL=/opt/robot-buddy/models/voice.onnx
```

This announces a fixed offline status locally, not model replies. Ordinary speech still streams from the server. Omit `BUDDY_PIPER_MODEL` to disable it. On the bench test loss and recovery of the backend and confirm mute and interruption silence the device.

### Camera, gallery and operator controls

With the service stopped check camera access as the service user: `runuser -u robot-buddy -- rpicam-hello --list-cameras`, then `runuser -u robot-buddy -- rpicam-vid --nopreview --frames 1 -t 0 --width 384 --height 288 --framerate 15 --codec yuv420 -o - | wc -c` — a full packed frame is **165888 bytes**. Use the `pi-vision` binary; matching occurs in-process with CPU OpenCV. Install the authorized YuNet 2023mar ONNX model for detection. Only for opt-in recognition also install SFace 2021dec. Both hashes match the verified public model table above and the [vision guide](apps/robot/src/vision/README.md), which also covers the private recognition fixture test. Fetch the actual LFS object, not its text pointer. No external vision worker is required.

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

## 5. Hardware bring-up checklist, upgrades and recovery

Before leaving a robot unattended, record the binary revision/checksum, OS and model versions, calibration file revision, and outcomes on **that robot**.

**Open release gates:** verify physical servo calibration and torque release, wake accuracy and echo rejection, tracking/recognition accuracy, and latency under simultaneous camera inference and 50 Hz neck motion. Follow the [loaded Pip measurement procedure](docs/performance-pip.md) (driven by `scripts/pip-perf.py`) and the release qualification gates in [docs/testing-pip-release.md](docs/testing-pip-release.md). The M4 Pro benchmark starts with a submitted transcript and uses simulated audio I/O; its numbers do not measure endpointing, Whisper model startup, an acoustic wake, or audible playback.

**What was verified before bench commissioning:** both the Bookworm private-prefix OpenCV 4.10 build and the Trixie packaged OpenCV build linked the full `aarch64-unknown-linux-gnu` release (`pi-vision,onnx-wake`) inside ARM64 Debian containers. The Trixie binary ran `setup-card` unprivileged, and its full-feature robot test suite passed (189 passed; the two tests requiring private recordings or labeled images were skipped). These checks do not establish motor direction, camera accuracy, wake behavior, timing, or memory headroom on a physical Pi.

> **Important distinction:** Every numeric threshold in the bring-up checklist below is a **proposed engineering go/no-go threshold**, **not** observed Raspberry Pi 5 performance. Record the actual measurements from the target robot beside each gate before declaring a robot ready.

### Step-by-step bring-up and go/no-go gates

| Gate | Operator procedure / command | Proposed engineering go/no-go threshold (not observed Pi performance) |
|---|---|---|
| **1. Preflight** | Run `vcgencmd get_throttled`, `ldd /usr/local/bin/robot-buddy-robot`, `systemd-analyze verify /etc/systemd/system/robot-buddy.service`, `systemctl show robot-buddy -p SupplementaryGroups -p MemoryMax -p TasksMax`, `stat -c '%U:%G %a' /etc/robot-buddy/device.identity.json /run/robot-buddy/control.sock`, and `ls -l /etc/polkit-1/rules.d/`. Verify `/ready` (`200`), OIDC startup discovery, full multi-origin browser claim + 6-digit confirmation, and cancelled IdP callback (`error=access_denied`). | `vcgencmd get_throttled` == `0x0`; `0` unresolved `ldd` libraries; `0` `systemd-analyze verify` warnings/errors; identity `robot-buddy:robot-buddy 600`; control socket `robot-buddy:pip-control 660`; `40-robot-buddy-nm.rules` installed with no legacy `50-robot-buddy-nm.rules` and no unintended site-policy override; drop-in `MemoryMax=1G` / `TasksMax=256` active; claim + cancel-error flow pass cleanly; cold boot, known-Wi-Fi reboot and uplink recovery preserve bot ID and ownership. |
| **2. Calibration & torque safety** | With head supported and service stopped, run `servo ping`, `servo read`, and `servo verify` as `robot-buddy`. Confirm face `hello`/`hb`, unplug/replug recovery, and board-reset QR/mood repaint. Start service, exercise a small bounded manual pose on `127.0.0.1:8091`, release it, then run `systemctl stop robot-buddy` and feel all three axes by hand. | `servo verify` passes with `0` warnings and `min_steps < center < max_steps` inside hand-checked cable clearance; EEPROM lock confirmed; `0` unexpected jumps or overtravel; **100% physical torque release confirmed on stop** (if unconfirmed, cut 12 V servo power immediately—no-go). |
| **3. Acoustic wake, echo & p95 latency** | With camera tracking and 50 Hz neck motion active simultaneously (verified in-window by `total_ticks > 0`, `0` `neck motion stopped` events, positive `vision_frames` delta, and `0` camera errors), run `scripts/pip-perf.py run` (`≥ 20` turns after `≥ 1` warm-up and `≥ 10` barge-ins, `0` failed trials) and `scripts/pip-perf.py compare --strict`, plus 10 minutes of continuous speech playback and mute checks. | Wake detection ≥ 90% true-positive on held-out quiet/room clips; `0` false wake triggers during 10 min playback (if neural barge-in false-triggers, set `BUDDY_LISTEN_BARGE_IN=0`); acoustic end-of-speech → first audible (`eos_to_first_audible_ms`) proposed p95 ≤ 3,000 ms (3.0 s); wake END → actual playback stop (`wake_to_playback_stop_ms`) proposed p95 ≤ 500 ms (0.5 s) for neural wake (`--wake-mode neural`) or ≤ 2,500 ms provisional for transcript wake (`--wake-mode transcript`); mute silences audio immediately; 50 Hz neck skipped 20 ms slots (`skipped_slots / total_ticks`) ≤ 0.1%; `allow_inactive=false`, `simulated=false`. |
| **4. Loaded memory, tasks & thermal soak** | During continuous simultaneous vision + speech + 50 Hz neck load, capture `scripts/pip-perf.py cgroup-snapshot`, `systemctl show robot-buddy -p NRestarts -p MemoryCurrent -p MemoryPeak -p TasksCurrent -p Result`, `systemd-cgtop`, and `vcgencmd measure_temp && vcgencmd get_throttled`. | `MemoryPeak` < 1 GiB (1,024 MiB) hard ceiling and ≤ 921.6 MiB (proposed 10% reserve under `MemoryMax=1G` in `20-pip.conf`); `0` OOM kills (`oom_kill == 0`); `TasksCurrent` / peak ≤ 200 (headroom under `TasksMax=256`); `NRestarts` delta == `0`, `0` watchdog timeouts, `0` panics or safety faults; `vcgencmd get_throttled == 0x0` (no under-voltage or thermal throttling) and SoC temperature < 80 °C under the Active Cooler. |
| **5. Camera tracking, recognition & operator controls** | With service stopped run `runuser -u robot-buddy -- rpicam-vid --nopreview --frames 1 -t 0 --width 384 --height 288 --framerate 15 --codec yuv420 -o - \| wc -c`. Restart service; verify face tracking direction and recenter, camera-absent recovery, and (only if `BUDDY_VISION_SFACE` is enabled) enroll 2 views of one consenting person, test held-out recognition and a different person, then test Forget across restart. Inspect `ss -tlnp` and test operator auth (`401`/`403`). | Packed frame size == **165888** bytes; tracking moves in the true viewer-frame direction (set `BUDDY_VISION_ROTATE_180=1` if inverted) and recenters cleanly when no face is present; optional SFace recognition proposed ≥ 90% true-accept on held-out views and `0%` false-accept on another person at the 0.42 threshold; Forget deletes all samples and persists across restart; operator panel binds `127.0.0.1:8091` only and refuses unauthenticated (`401`) and unclaimed/wrong-origin (`403`) requests. |
| **6. Upgrade & rollback drill** | Record pre-upgrade binary SHA-256, bot ID, ownership, calibration revision and gallery state; back up binary, `device.env`, unit/drop-in, polkit rule and `neck.json`. Perform an upgrade (see below), verify identity/state continuity, then execute a full rollback to the saved binary/config and verify `servo verify` before rearming. | Both upgrade and rollback preserve the same bot ID, ownership, cache and gallery without re-running `init` or `provision`; `ldd` and `systemd-analyze verify` exit `0`; `servo verify` passes before rearming; `0` unverified servo armings and `0` credential leaks in journals. |

For troubleshooting, inspect `systemctl show robot-buddy -p NRestarts -p MemoryCurrent -p MemoryPeak -p TasksCurrent -p Result`, `systemd-cgtop`, `journalctl -u robot-buddy -b`, and `vcgencmd get_throttled`. For camera/ALSA permissions inspect groups on actual nodes; for absent ML dependencies check `ldd`, `pkg-config --modversion opencv4` (if development packages installed), executable paths and model file permissions. A `403` after valid operator credentials requires a connected, claimed bot and the expected origin; `401` means credentials are missing or wrong. The operator panel does not bind a non-loopback address.

**Upgrade:** save the current working binary, root-only env file, unit/drop-ins, polkit rule and calibration. Stop the service; replace the binary and any changed unit/rules (install `40-robot-buddy-nm.rules` after checking site polkit policy and remove any legacy `/etc/polkit-1/rules.d/50-robot-buddy-nm.rules`), verify the new binary's dependencies (`ldd`) and unit (`systemd-analyze verify`), run `systemctl daemon-reload` and restart. Keep `/etc/robot-buddy/device.identity.json`, `/var/lib/robot-buddy/` (cache, setup card, gallery) and the server's volume/secrets. Do not re-run `init`, `provision` or overwrite the env file during an upgrade. To roll back, stop the unit, restore the saved binary and matching configuration/unit/polkit rule/calibration, reload and restart. If EEPROM limits changed, reconcile them **with power to the motors under operator control** and verify before rearming. A device rollback does not roll back server state. Removing optional hardware flags is the quickest way to isolate a peripheral failure without changing identity.
