# Deploy Robot Buddy on a Raspberry Pi 5

The Pi runs `robot-buddy-robot`. A separate host runs the Rust server and Rivet engine. Bring up identity, network and playback first, then enable each peripheral on its own after you check it. The Pi never needs the issuer key, the Rivet admin token, the OIDC secret or the model-provider key. [NOTICE.md](NOTICE.md) covers provenance and licensing. The neck choreography, expression poses and local voice coloring were adapted from butty in collaboration with ToschLogic. That is not an MIT relicense or a redistribution grant. Models, voices, firmware sources, recordings and the private reference tree are **not** in the Pi binary or in this repository.

Sections, in order: [0 hardware and what to get first](#0-what-you-need-before-you-start), [1 backend and identity](#1-bring-up-the-backend-and-issue-one-device-identity), [2 binary](#2-build-the-linux-arm64-binary), [3 Pi and service](#3-prepare-the-pi-and-install-the-service), [4 peripherals and models](#4-enable-hardware-one-subsystem-at-a-time), [5 bring-up checklist](#5-hardware-bring-up-checklist), [troubleshooting](#troubleshooting), [upgrade and rollback](#upgrade-and-rollback).

## 0. What you need before you start

**From Brittian.** None of these are in this repo:

- Access to the private **butty** repository. It holds the ESP32-S3 face firmware (`firmware/`, a PlatformIO project), the parametric CAD (`cad/`, `build.py`), the assembly guide (`docs/body-assembly.*`), the parts list (`SHOPPING.md`), the spec sheet (`SPEC.md`) and the rev-2 fit notes (`REV2.md`). Its `SPEC.md` names it `ToschLogic/butty` on GitHub ("ask for access").
- The private `hey_pip.onnx` neural wake classifier, plus its SHA-256 from the owner's records. It is needed only for neural wake and spoken barge-in.
- Accounts and credentials for the server host: an OpenRouter API key (or your own OpenAI-compatible model endpoint), an OIDC client registered for `<BUDDY_PUBLIC_URL>/auth/callback`, and optionally a Brave Search key. Also two public HTTPS origins behind a TLS reverse proxy.
- An assembled Pip, or the parts below.

**Hardware.** This is a summary of butty `SHOPPING.md`, `SPEC.md` and `firmware/README.md`. Prices, links and the full screw and filament list are there.

| Area | Part | Connects to |
| --- | --- | --- |
| Computer | Raspberry Pi 5 with the official Active Cooler, 64 GB microSD | (butty `SHOPPING.md` lists the 4 GB Pi, `SPEC.md` the 8 GB; the robot's cgroup budget is 1 GiB either way) |
| Face | ESP32-S3-DevKitC-1 **N16R8**, 2 × GC9A01 1.28 in round LCDs under 30 mm glass domes, 2 × MG90S ear servos, WS2812B 12-LED chest ring | Pi USB, to the board's native **USB** port (GPIO 19/20), not UART |
| Neck | 3 × Feetech **STS3215** 12 V serial bus servos (yaw, pitch, roll) and a USB serial bus servo driver board (WCH/CH343-class, USB vendor `1a86`) | Pi USB, 1 Mbaud. 12 V servo power |
| Camera | **OV5647** 160° wide-angle CSI module, with a 22→15-pin Pi 5 camera cable | Pi CSI port |
| Microphone | Seeed **reSpeaker**, USB. butty `SHOPPING.md` lists the ReSpeaker Lite (USB, 2-mic), while its `SPEC.md` and assembly README name the reSpeaker Flex (XVF3800). Confirm which with Brittian | Pi USB, 2-channel 16 kHz capture |
| Speaker | **MAX98357A** I2S amp and a 3 W speaker | Pi GPIO: BCLK 18, LRCLK 19, DIN 21 |
| Power | 12 V 5 A supply (5.5 × 2.1 mm), 2 × 12 V→5 V 5 A **plain** USB-C buck converters (not USB-PD), a 12 V 5 A panel switch | see wiring below |

**Wiring and power** (butty `firmware/README.md`):

```text
12 V in ──┬── buck A (5 V 5 A) ── Pi 5 USB-C only
          ├── buck B (5 V 5 A) ── MG90S ear servos V+, WS2812B ring 5V
          └── servo bus adapter (12 V) ── 3 × STS3215
Pi USB ── ESP32-S3 native USB (rev 1: a cut USB cable soldered to 5V, GND, GPIO19 D-, GPIO20 D+)
GND: buck A, buck B, ESP32, ring and ear servos all share ground
```

- Never connect the Pi or ESP32 USB 5 V to buck B. Never power the ear servos from the ESP32's 5 V pin: a stall pulls about 700 mA and resets the board.
- The eye, ear and LED pin map (GC9A01 SPI on GPIO 9–14, 17, 18, 21; ears on 4 and 5; ring data on 6 through 330 Ω) is in butty `firmware/README.md` and `include/pins.h`.
- Keep the 12 V servo switch within reach. Software cannot confirm torque-off after a power loss, a broken cable or a killed process.
- Rev-1 fit issues, including the head hitting a stop at about +21.5° pitch-up, are in butty `REV2.md`. Measure your own build's limits (§4).

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

Use **64-bit Raspberry Pi OS**, **Bookworm** (Debian 12) or **Trixie** (Debian 13), arm64. Check with `uname -m` → `aarch64` and `dpkg --print-architecture` → `arm64`. The ARM64 [toolchain matrix](infra/toolchain-validation.md) passed on both at `82e8d5b`, with default and full Pip features. Bookworm runs every peripheral without an OS upgrade if you use the native Bookworm build below. On Trixie, use its packaged OpenCV or the ARM64 Buildx recipe. The binary needs the same OpenCV and glibc ABI it was built against, so do **not** copy a Trixie binary onto Bookworm. Models are installed later, on the device.

**Recommended for a first Pip: Raspberry Pi OS Trixie with the soaked binary.** The 30-minute E2E soak at `82e8d5b` (see [CHANGELOG.md](CHANGELOG.md#validation-status)) ran a robot ELF with SHA-256 `be7cae4e42fda047f857d327af875e0dbf91b54af3a8076df84080e802e2768b` (GNU BuildID `c15c88b7594bd0919c5a0bf78609c2b01c6162e9`). It comes from the `build-robot` stage of the [pip-e2e image](tests/pip-e2e/Dockerfile): `rust:1.100.0-slim-trixie`, `--features pi-vision,onnx-wake`, `aarch64-unknown-linux-gnu`. It links Trixie's packaged OpenCV 4.10 (`libopencv_*.so.410`). Since `82e8d5b`, only docs and bench files have changed, so building at the branch tip uses the same sources. To get the ELF:

```sh
tests/pip-e2e/run-in-docker.sh build                    # image robot-buddy-pip-e2e:arm64 (override with PIP_E2E_IMAGE)
docker create --name pip-elf robot-buddy-pip-e2e:arm64
docker cp pip-elf:/usr/local/bin/robot-buddy-robot target/pip-e2e-robot-buddy-robot
docker rm pip-elf
sha256sum target/pip-e2e-robot-buddy-robot              # expect be7cae4e...2768b
```

`infra/pi-build.Dockerfile` runs the same Trixie build command with the same base-image digest. It has not been checked byte for byte against the soaked ELF at `82e8d5b`. TODO(Brittian): confirm that a clean rebuild of either recipe reproduces `be7cae4e…2768b`, and what to do if it does not. Record the SHA-256 you deploy, either way.

| Where | Build from the repository root | Artifact |
|---|---|---|
| Docker Buildx (`linux/arm64`), the soaked recipe | E2E image commands above | `target/pip-e2e-robot-buddy-robot` |
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

Flash **Raspberry Pi OS Lite (64-bit)**, Bookworm or Trixie, to match the binary you built in §2. Enable SSH on a trusted provisioning network. The Wi-Fi interface must support AP mode under NetworkManager. Keep initial access over Ethernet if you can. Run the administrative steps below as root. If the Pi previously ran butty's Python brain, stop its `pip.service` (`systemctl disable --now pip.service`). Two processes must never own the mic, serial ports or camera at the same time.

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

* **Face firmware:** this repo does not contain it. It is the PlatformIO project in the private butty repo's `firmware/` directory (Arduino framework, `platform = espressif32@6.9.0`, board `esp32-s3-devkitc-1` with 16 MB QIO flash and 8 MB octal PSRAM, default env `pip-face`). Pip's client speaks its line protocol: Pi → board `{"e":…,"i":…}`, `{"look":[x,y]}`, `{"led":…}`, `{"qr":[…],"w":…}`, `{"ping":n}`; board → Pi `{"hello":"pip-face",…}` at boot and a `{"hb":…}` heartbeat every second. With no valid line for 5 s the board falls back to a calm idle face. To flash it from a Mac or the Pi:

  ```sh
  pip install platformio            # or: uv tool install platformio (butty firmware/README.md)
  cd <butty checkout>/firmware
  pio run                           # build the default pip-face env
  pio run -t upload                 # flash over the board's native USB port
  pio device monitor                # 115200; expect a hello line, then one hb per second; close it afterwards
  ```

  On the Pi, **stop the service first** (`systemctl stop robot-buddy`) so it does not hold the CDC-ACM port open. Use the port labelled **USB** (native USB), not UART. If upload does not enter download mode, enter the ROM bootloader by hand (**hold `BOOT`, tap `RST`, release `BOOT`**), run `pio run -t upload` again, and tap `RST` afterwards if needed. If the eyes stay dark, butty's `firmware/README.md` lists the diagnostic envs (`pin-check`, `bitbang-test`, `screen-test`, `wire-check`, each flashed with `pio run -e <name> -t upload`). Ear centre and direction are compile-time values (`EAR_CENTER_US_L/R`, `EAR_DIR_L/R` in `include/pins.h`), calibrated with `{"e":"neutral","i":1}` and `{"e":"surprised","i":1}` from the monitor.
* **Face on the Pi:** pick the board's path from `ls -l /dev/serial/by-id/` (Espressif USB vendor `303a`) and set `BUDDY_FACE_SERIAL` to it. The client sets raw 115200 on every open. Check both eyes, the ring, ear movement and the heartbeat. Then unplug and replug the face and confirm it recovers.
* **Neck:** the CH343/STS3215 adapter has a **different** serial path. If an adapter lacks a unique ID, use a stable USB topology path under `/dev/serial/by-path/`. With the service stopped and the head supported, give each servo a distinct ID in position mode, one servo on the bus at a time with 12 V on the adapter. STS3215s ship with ID 1. butty's convention is yaw 1, pitch 2, roll 3. `robot-buddy-robot servo` has no ID-assignment command, so use the Feetech vendor tool or butty's `brain` servo tool (`servo_tool set-id`, see butty `brain/README.md`). Record its straight-ahead raw position, inversion and conservative cable-safe range on the assembled robot. Do not copy calibration between robots.

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

Voice activity detection is built into the client (`apps/robot/src/listen/vad.rs`, energy-based, 20 ms frames, clip ends after 800 ms of quiet). There is no VAD model to install. Install [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on the Pi. The harness validated **v1.9.6** (source tarball SHA-256 in the table below), built the way [`install-speech-stack.sh`](tests/pip-e2e/speech/install-speech-stack.sh) does it: `cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DWHISPER_BUILD_EXAMPLES=ON -DWHISPER_BUILD_TESTS=OFF -DWHISPER_SDL2=OFF -DGGML_NATIVE=OFF -DGGML_OPENMP=ON`, then `cmake --build build -j2 --target whisper-cli` and `install -m755 build/bin/whisper-cli /usr/local/bin/`. The client passes `-of /dev/null`, so whisper.cpp 1.8 or newer writes the transcript to stdout. Select a local, license-reviewed English ggml model, e.g. `ggml-tiny.en.bin`. For every model in this guide, verify the pinned revision and publisher checksum **before installation**; keep the license/model card with the device inventory. The service does not download weights on first boot. Install models read-only by the service under `/opt/robot-buddy/models`, outside `/home`.

```ini
BUDDY_LISTEN_MODEL=/opt/robot-buddy/models/ggml-tiny.en.bin
BUDDY_LISTEN_WHISPER=/usr/local/bin/whisper-cli
BUDDY_LISTEN_DEVICE=plughw:CARD=YOUR_MIC,DEV=0
BUDDY_LISTEN_CHANNELS=2
BUDDY_LISTEN_CHANNEL=0
```

`CHANNELS` is the captured interleaved channel count, `CHANNEL` the zero-based beam index. With transcript wake, say “Hey Pip, what time is it?” and test a follow-up after the reply. **Spoken barge-in requires neural wake:** transcript mode does not transcribe playback echo to search for “Hey Pip”, even when `BUDDY_LISTEN_BARGE_IN=1`. With neural wake enabled, test an interruption during speech; set `BUDDY_LISTEN_BARGE_IN=0` if the beam/amp routing does not reject echo. The external socket still works without this listener: put only the external STT account in `pip-control` (not `robot-buddy`). Test as that account: `printf '{"wake":true}\n' | socat - UNIX-CONNECT:/run/robot-buddy/control.sock`. Do not enable two capture processes on the same microphone.

**Optional neural wake:** the full image includes the `onnx-wake` Cargo feature. Install an AArch64 Linux CPU build of [ONNX Runtime](https://github.com/microsoft/onnxruntime/releases). The validated build is **1.20.1**: the `runtimes/linux-arm64/native/` libraries from the `Microsoft.ML.OnnxRuntime` 1.20.1 NuGet package, which match the `ort =2.0.0-rc.9` crate's `ORT_API_VERSION=20`. Checksums are in the table below. Point `ORT_DYLIB_PATH` at the actual `libonnxruntime.so` file, not a directory, and keep `libonnxruntime_providers_shared.so` beside it. Use a local directory containing the authorized private `hey_pip.onnx` classifier plus the matching public openWakeWord `melspectrogram.onnx` and `embedding_model.onnx` frontend (`v0.5.1`). Check each model's terms, especially for commercial use; no weights are supplied and no hash is published here for the private `hey_pip.onnx` asset (verify it against the authorized owner's provisioning record). Set e.g. `BUDDY_LISTEN_WAKE_DIR=/opt/robot-buddy/models/wake` alongside `BUDDY_LISTEN_MODEL`. This changes wake detection, not speech transcription. Validate quiet, noisy, positive and negative recorded clips against the reference before enabling it unattended; remove `BUDDY_LISTEN_WAKE_DIR` to return to transcript wake.

#### Verified public model pins

Check every file's SHA-256 (`sha256sum`) before you copy it to `/opt/robot-buddy/models` or install it. Fetch the real LFS bytes, not the text pointers. The public pins match [`tests/pip-e2e/speech/assets.lock.json`](tests/pip-e2e/speech/assets.lock.json) and [`fetch-vision-models.sh`](tests/pip-e2e/peripherals/fetch-vision-models.sh), which is what the E2E harness and soak used.

| File | Upstream pin | Size (bytes) | SHA-256 |
|---|---|---:|---|
| `whisper.cpp-v1.9.6.tar.gz` (source, builds `whisper-cli`) | `ggml-org/whisper.cpp` tag `v1.9.6` archive | | `7ab15dc88022d2425ad94dcffd8abbf1b6d7018089b4e48d6b40780738e0b445` |
| `microsoft.ml.onnxruntime.1.20.1.nupkg` | NuGet `Microsoft.ML.OnnxRuntime` 1.20.1 | | `9c32108e9a1c78b090a95c2a219f244dd5f067d4caf68fb0f967103f3f20839c` |
| ↳ `runtimes/linux-arm64/native/libonnxruntime.so` | extracted from the nupkg | | `f8407a59e7e4c4dddf3b6e01198a24444aa66ac4ad93b4b4284b6e98d1f938b1` |
| ↳ `runtimes/linux-arm64/native/libonnxruntime_providers_shared.so` | extracted from the nupkg | | `d93c6b9016eb23a46b7ace5a6c7d56139fdf11bfe0228e93801de758707dee2b` |
| `ggml-tiny.en.bin` | `ggerganov/whisper.cpp` on Hugging Face (`5359861c739e955e79d9a303bcbc70fb988958b1`; the harness fetches `resolve/main` and checks this hash) | 77,704,715 | `921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f` |
| `melspectrogram.onnx` | `dscripka/openWakeWord` release `v0.5.1` | 1,087,958 | `ba2b0e0f8b7b875369a2c89cb13360ff53bac436f2895cced9f479fa65eb176f` |
| `embedding_model.onnx` | `dscripka/openWakeWord` release `v0.5.1` | 1,326,578 | `70d164290c1d095d1d4ee149bc5e00543250a7316b59f31d056cff7bd3075c1f` |
| `face_detection_yunet_2023mar.onnx` | `opencv/opencv_zoo` (`48b10fa505aeb51fbd127fc60a5c660040e823b7`) | 232,589 | `8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4` |
| `face_recognition_sface_2021dec.onnx` | `opencv/opencv_zoo` (`48b10fa505aeb51fbd127fc60a5c660040e823b7`) | 38,696,353 | `0ba9fbfa01b5270c96627c4ef784da859931e02f04419c829e83484087c34e79` |
| `hey_pip.onnx` (private, from Brittian) | butty's custom-trained openWakeWord classifier | | TODO(Brittian): publish the expected SHA-256 privately to the operator. Not stored in this repo |
| Piper voice `.onnx` + `.onnx.json` | operator's choice. butty's `voice/pip_voice.py` uses `en_US-ryan-medium`. Its `brain/README.md` downloads `en_US-libritts_r-medium` from `rhasspy/piper-voices` | | TODO(Brittian): no voice or Piper version is pinned or tested in this repo. Choose one and record its checksum |

The wake directory (`BUDDY_LISTEN_WAKE_DIR`) must contain these three file names: `melspectrogram.onnx`, `embedding_model.onnx` and `hey_pip.onnx`.

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

## 5. Hardware bring-up checklist

Include speaker alerts and music in the negative wake set, not just room noise and conversation. An ARM64 fixture with the selected classifier crossed the busy threshold on an artificial two-tone signal. That identifies a test case, not a false-trigger rate for the assembled robot. Disable acoustic interruption if playback or alerts wake Pip during the physical check.

Before leaving a robot unattended, record the binary revision/checksum, OS and model versions, calibration file revision, and outcomes on **that robot**.

**Open release gates:** verify physical servo calibration and torque release, wake accuracy and echo rejection, tracking/recognition accuracy, and latency under simultaneous camera inference and 50 Hz neck motion. Follow the [loaded Pip measurement procedure](docs/performance-pip.md) (driven by `scripts/pip-perf.py`) and the release qualification gates in [docs/testing-pip-release.md](docs/testing-pip-release.md). The M4 Pro benchmark starts with a submitted transcript and uses simulated audio I/O; its numbers do not measure endpointing, Whisper model startup, an acoustic wake, or audible playback.

**What was verified without hardware, at `82e8d5b`:**

- The [ARM64 matrix](infra/toolchain-validation.md) built, linted and tested on Bookworm (private OpenCV 4.10 prefix) and Trixie (packaged OpenCV 4.10). The full-feature robot suite had 254 passed and 2 ignored. The ignored tests need private recordings or labelled images.
- The 30-minute virtual-Pip [E2E soak](tests/pip-e2e/README.md) passed. Results are in [CHANGELOG.md](CHANGELOG.md#validation-status).
- A fresh ARM64 systemd [deployment rehearsal](tests/pip-deploy-qa/EVIDENCE.md) passed on the earlier candidate `5351762`: install, the 1 GiB drop-in, OIDC claim, upgrade and rollback.

None of this establishes motor direction, camera accuracy, acoustic wake behaviour, audible timing, thermals or memory headroom on a physical Pi. The gates below do.

> **Important distinction:** Every numeric threshold in the bring-up checklist below is a **proposed engineering go/no-go threshold**, **not** observed Raspberry Pi 5 performance. Record the actual measurements from the target robot beside each gate before declaring a robot ready.

### Step-by-step bring-up and go/no-go gates

| Gate | Operator procedure / command | Proposed engineering go/no-go threshold (not observed Pi performance) |
|---|---|---|
| **1. Preflight** | Run `vcgencmd get_throttled`, `ldd /usr/local/bin/robot-buddy-robot`, `systemd-analyze verify /etc/systemd/system/robot-buddy.service`, `systemctl show robot-buddy -p SupplementaryGroups -p MemoryMax -p TasksMax`, `stat -c '%U:%G %a' /etc/robot-buddy/device.identity.json /run/robot-buddy/control.sock`, and `ls -l /etc/polkit-1/rules.d/`. Verify `/ready` (`200`), OIDC startup discovery, full multi-origin browser claim + 6-digit confirmation, and cancelled IdP callback (`error=access_denied`). | `vcgencmd get_throttled` == `0x0`; `0` unresolved `ldd` libraries; `0` `systemd-analyze verify` warnings/errors; identity `robot-buddy:robot-buddy 600`; control socket `robot-buddy:pip-control 660`; `40-robot-buddy-nm.rules` installed with no legacy `50-robot-buddy-nm.rules` and no unintended site-policy override; drop-in `MemoryMax=1G` / `TasksMax=256` active; claim + cancel-error flow pass cleanly; cold boot, known-Wi-Fi reboot and uplink recovery preserve bot ID and ownership. |
| **2. Calibration & torque safety** | With head supported and service stopped, run `servo ping`, `servo read`, and `servo verify` as `robot-buddy`. Confirm face `hello`/`hb`, unplug/replug recovery, and board-reset QR/mood repaint. Start service, exercise a small bounded manual pose on `127.0.0.1:8091`, release it, then run `systemctl stop robot-buddy` and feel all three axes by hand. | `servo verify` passes with `0` warnings and `min_steps < center < max_steps` inside hand-checked cable clearance; EEPROM lock confirmed; `0` unexpected jumps or overtravel; **100% physical torque release confirmed on stop** (if unconfirmed, cut 12 V servo power immediately—no-go). |
| **3. Acoustic wake, echo & p95 latency** | With camera inference and neck motion active, run the [measurement procedure](docs/performance-pip.md): ≥20 turns after warm-up, no failed trials, ≥10 interruptions in **neural** mode, and ten minutes of playback echo and mute checks. Confirm increasing camera frames and neck ticks throughout. | Proposed: ≥90% held-out wake detections; zero false triggers during playback; end-of-speech → first audible p95 ≤3 s; neural wake END → actual playback stop p95 ≤500 ms; skipped 20 ms slots ≤0.1%. Transcript-wake barge latency is **not applicable**: verify no playback echo initiates a decode or turn, then verify the first post-playback wake. Missing hardware activity, simulated samples, and muted/disabled trials cannot qualify. |
| **4. Loaded memory, tasks & thermal soak** | During continuous simultaneous vision + speech + 50 Hz neck load, capture `scripts/pip-perf.py cgroup-snapshot`, `systemctl show robot-buddy -p NRestarts -p MemoryCurrent -p MemoryPeak -p TasksCurrent -p Result`, `systemd-cgtop`, and `vcgencmd measure_temp && vcgencmd get_throttled`. | `MemoryPeak` < 1 GiB (1,024 MiB) hard ceiling and ≤ 921.6 MiB (proposed 10% reserve under `MemoryMax=1G` in `20-pip.conf`); `0` OOM kills (`oom_kill == 0`); `TasksCurrent` / peak ≤ 200 (headroom under `TasksMax=256`); `NRestarts` delta == `0`, `0` watchdog timeouts, `0` panics or safety faults; `vcgencmd get_throttled == 0x0` (no under-voltage or thermal throttling) and SoC temperature < 80 °C under the Active Cooler. |
| **5. Camera tracking, recognition & operator controls** | With service stopped run `runuser -u robot-buddy -- rpicam-vid --nopreview --frames 1 -t 0 --width 384 --height 288 --framerate 15 --codec yuv420 -o - \| wc -c`. Restart service; verify face tracking direction and recenter, camera-absent recovery, and (only if `BUDDY_VISION_SFACE` is enabled) enroll 2 views of one consenting person, test held-out recognition and a different person, then test Forget across restart. Inspect `ss -tlnp` and test operator auth (`401`/`403`). | Packed frame size == **165888** bytes; tracking moves in the true viewer-frame direction (set `BUDDY_VISION_ROTATE_180=1` if inverted) and recenters cleanly when no face is present; optional SFace recognition proposed ≥ 90% true-accept on held-out views and `0%` false-accept on another person at the 0.42 threshold; Forget deletes all samples and persists across restart; operator panel binds `127.0.0.1:8091` only and refuses unauthenticated (`401`) and unclaimed/wrong-origin (`403`) requests. |
| **6. Upgrade & rollback drill** | Record pre-upgrade binary SHA-256, bot ID, ownership, calibration revision and gallery state; back up binary, `device.env`, unit/drop-in, polkit rule and `neck.json`. Perform an upgrade (see below), verify identity/state continuity, then execute a full rollback to the saved binary/config and verify `servo verify` before rearming. | Both upgrade and rollback preserve the same bot ID, ownership, cache and gallery without re-running `init` or `provision`; `ldd` and `systemd-analyze verify` exit `0`; `servo verify` passes before rearming; `0` unverified servo armings and `0` credential leaks in journals. |

## Troubleshooting

Start with `journalctl -u robot-buddy -b`, `systemctl show robot-buddy -p NRestarts -p MemoryCurrent -p MemoryPeak -p TasksCurrent -p Result`, `systemd-cgtop` and `vcgencmd get_throttled`. The quickest way to isolate a failing peripheral, without changing identity, is to comment out its variables in `device.env` and restart.

| Symptom | Check |
| --- | --- |
| Server container never becomes ready | `docker compose logs`. Startup fails on a missing `AI_MODEL`, on HTTP to a non-loopback `AI_BASE_URL` without `AI_ALLOW_PLAINTEXT_MODEL=1`, on an unreachable or redirecting OIDC discovery document, or on an `issuer` that does not match `OIDC_ISSUER` exactly |
| Robot cannot enroll or connect | `BUDDY_SERVER_URL` must be HTTPS and reachable from the Pi. The TLS proxy must pass WebSocket upgrades to `:6420`. `/etc/robot-buddy/device.identity.json` must be `robot-buddy:robot-buddy 600`, because looser modes are refused at start |
| Binary will not start, `ldd` shows `not found` | OS/ABI mismatch (a Trixie ELF on Bookworm, or a missing OpenCV runtime or `/etc/ld.so.conf.d/robot-buddy-opencv.conf` + `ldconfig`). Rebuild for the target OS (§2) |
| No setup hotspot | NetworkManager running, AP-capable `BUDDY_WIFI_INTERFACE`, `40-robot-buddy-nm.rules` installed and no stricter site polkit rule overriding it |
| No sound | `BUDDY_SPEECH=true`. `BUDDY_ALSA_DEVICE` must name the real `plughw:CARD=…,DEV=0` from `aplay -L`, not a desktop PipeWire default. `dtoverlay=max98357a` with `dtparam=audio=off`. `TTS_MODEL=elevenlabs/eleven-v4-turbo` must be set on the server |
| Never wakes / never transcribes | Capture test as `robot-buddy` (§4). Channels and beam (`BUDDY_LISTEN_CHANNELS`, `BUDDY_LISTEN_CHANNEL`). `whisper-cli` and model paths readable by the service. For neural wake: `ORT_DYLIB_PATH` names the `.so` file, and all three ONNX files are present |
| Face dark or not responding | Native USB port, not UART. The service is stopped while flashing, and `pio device monitor` is closed afterwards. `BUDDY_FACE_SERIAL` is the by-id path. Heartbeats appear in the monitor. Use the firmware diagnostic envs (§4) |
| Neck refuses to arm | `servo verify` must pass: position mode, locked EEPROM limits matching `neck.json`, `min_steps < center < max_steps`. Never widen limits to make it pass |
| Camera absent | `rpicam-hello --list-cameras` as `robot-buddy`. `video` group on both the user and `20-pip.conf`. `camera_auto_detect=0` + `dtoverlay=ov5647,cam0` if needed |
| Operator panel `401` / `403` | `401`: missing or wrong credentials. `403` after valid credentials: the bot must be connected and claimed, and the request must come from the expected origin. The panel only binds loopback, so use an SSH tunnel |
| Restarts or OOM | `MemoryPeak` against the 1 GiB drop-in, `vcgencmd get_throttled` (under-voltage means buck A is not holding 5 A at about 5.1 V) |

## Upgrade and rollback

**Before any upgrade**, save the working binary and its SHA-256, the root-only env file, the unit and drop-ins, the polkit rule and `neck.json`. Record the bot ID, ownership, calibration revision and gallery state.

**Upgrade:** stop the service. Replace the binary and any changed unit or rules. Install `40-robot-buddy-nm.rules` after checking site polkit policy, and remove any legacy `/etc/polkit-1/rules.d/50-robot-buddy-nm.rules`. Check the new binary's dependencies (`ldd`) and the unit (`systemd-analyze verify`), then run `systemctl daemon-reload` and restart. Keep `/etc/robot-buddy/device.identity.json`, `/var/lib/robot-buddy/` (cache, setup card, gallery) and the server's volume and secrets. Do not re-run `init` or `provision`, and do not overwrite the env file, during an upgrade.

**Rollback:** stop the unit. Restore the saved binary and its matching configuration, unit, polkit rule and calibration. Then reload and restart. If EEPROM limits changed, reconcile them **with the motors powered, under operator control**, and run `servo verify` before rearming. A device rollback does not roll back server state. The server rolls back separately: stop the container, restore the `/data` volume backup together with its matching secrets, and start it again. The [rehearsal evidence](tests/pip-deploy-qa/EVIDENCE.md) shows a cross-version upgrade and rollback that kept identity and state.
