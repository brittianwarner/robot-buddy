# Deploy Robot Buddy on a Raspberry Pi 5

The Pi runs `robot-buddy-robot`. A separate host runs the Rust server and Rivet engine. Bring up identity, network and playback first, then enable each peripheral on its own after you check it. The Pi never needs the issuer key, the Rivet admin token, the OIDC secret or the model-provider key. [NOTICE.md](NOTICE.md) covers provenance and licensing. The neck choreography, expression poses and local voice coloring were adapted from ToschLogic's butty project in collaboration with ToschLogic. That is not an MIT relicense or a redistribution grant. Models, voices, firmware sources, recordings and butty files are **not** in the Pi binary or in this repository.

**Coming from butty?** butty runs a Python brain (`brain/`, systemd `pip.service`) on the Pi. robot-buddy replaces it with one Rust service, `robot-buddy-robot`, and moves the thinking and spoken replies to a separate server. The face firmware, CAD and wiring stay butty's. The [README comparison](README.md#butty-and-robot-buddy) lists every difference.

Sections, in order: [0 hardware and what to get first](#0-what-you-need-before-you-start), [1 backend and identity](#1-bring-up-the-backend-and-issue-one-device-identity), [2 binary](#2-build-the-linux-arm64-binary), [3 Pi and service](#3-prepare-the-pi-and-install-the-service), [4 peripherals and models](#4-enable-hardware-one-subsystem-at-a-time), [5 bring-up checklist](#5-hardware-bring-up-checklist), [troubleshooting](#troubleshooting), [upgrade and rollback](#upgrade-and-rollback).

## 0. What you need before you start

### From butty

Pip's hardware design, face firmware and wake model live in **butty**, ToschLogic's repository at <https://github.com/ToschLogic/butty>. Gage manages access to it. This guide cites butty at commit `c0ca680`. From your own butty checkout you need:

- `firmware/`: the ESP32-S3 face firmware, a PlatformIO project. Flash it in §4.
- `brain/models/wake/hey_pip.onnx`: the "Hey Pip" classifier, needed only for neural wake and spoken barge-in. Its SHA-256 is pinned in the [model table](#verified-model-pins).
- Reference only: `SPEC.md` (spec sheet), `REV2.md` (rev-1 fit issues), `README.md` (assembly), `firmware/README.md` and `firmware/include/pins.h` (pin map and wiring), `brain/config.yaml` and `brain/README.md` (servo IDs, Pi `config.txt`, audio devices), `cad/params.py` and `build.py` (CAD).

Nothing from butty is copied into this repository ([NOTICE.md](NOTICE.md)).

### What you create

- **An OpenRouter API key.** Create it at <https://openrouter.ai/keys>. Put it in the server's `.env` as `AI_API_KEY=<your-openrouter-key>`; that is the variable the server reads (`crates/buddy/src/model.rs`). The same key pays for spoken replies (`TTS_MODEL=elevenlabs/eleven-v4-turbo`): the server reuses `AI_API_KEY` for speech when `AI_PROVIDER=openrouter` and `TTS_API_KEY` is unset (`apps/server/src/speech.rs`). There is no separate ElevenLabs credential.
- **Or an OpenAI-compatible endpoint.** Set `AI_PROVIDER=openai_compatible` and `AI_BASE_URL`, plus `AI_API_KEY` if your endpoint needs one. Spoken replies then need `TTS_API_KEY=<your-openrouter-key>`, because the server reuses `AI_API_KEY` for speech only with `AI_PROVIDER=openrouter`.
- The server secrets and this robot's identity, from `init` and `provision` in §1.

### Decide with Brittian: reuse his backend, or run your own

This is the only question that needs Brittian. Everything else in this guide comes from this repository, your butty checkout, or your own Pip.

- **Run your own backend.** You need a domain and a public-CA certificate for two names ([TLS in front of the server](#tls-in-front-of-the-server) has a Caddy recipe). Run `init` fresh in §1, point the two HTTPS names at a TLS proxy in front of `:8080` (`BUDDY_PUBLIC_URL`) and `:6420` (`BUDDY_PUBLIC_RIVET_URL`), and set up OIDC (below).
- **Reuse Brittian's running backend.** Brittian runs `provision` with his issuer key and sends you two things: this robot's device identity file, privately (it holds the robot's private key), and his two public URLs, `BUDDY_PUBLIC_URL` (registration) and `BUDDY_PUBLIC_RIVET_URL` (Rivet). His OIDC is already configured, so you claim by signing in through his provider. On the Pi, set `BUDDY_SERVER_URL` to his `BUDDY_PUBLIC_URL` and install the identity file (§3); those are the only two values the robot reads for its server (`apps/robot/src/config.rs`, [infra/device.env.example](infra/device.env.example)). The robot learns the Rivet URL from the server at enrollment, so you need it only to check that the Pi can reach it. You need no access to his host and never the issuer key. Skip §1, and do not run `init` or `provision` yourself.

**Set up OIDC owner sign-in before bring-up.** The server starts with `OIDC_ISSUER` empty (`apps/server/src/config.rs`), and Pip still enrolls and connects. But then nobody can claim the robot, and an unclaimed Pip does not talk or open its operator panel:

- While connected, it refuses every utterance. The journal shows `utterance refused` with `reason="robot not owned"`, and the eyes flash the sorry face (`apps/robot/src/device.rs`). Pip answers only once it has an owner.
- The operator panel on `127.0.0.1:8091` answers every request that passes its password check with `403 Robot must be connected and claimed.` (`apps/robot/src/screen/operator.rs`).

Several §4 and §5 checks need the operator panel or a reply, so OIDC is part of the recommended bring-up path. [What needs a claim](#what-needs-a-claim) in §5 lists those checks, and the claim-free checks you can run first. To turn OIDC on with any standard provider (Google, Auth0, Okta, Keycloak and so on):

1. Register one web application. Redirect URI: `<BUDDY_PUBLIC_URL>/auth/callback`. Scopes: `openid profile`. The server uses the authorization-code flow with PKCE S256 (`apps/server/src/claim.rs`) and needs RS256 or ES256 ID tokens (`apps/server/src/oidc.rs`). It reads only the `sub` claim.
2. On the server host, set `OIDC_ISSUER=<issuer URL>`, `OIDC_CLIENT_ID=<client id>` and, for a confidential client, `OIDC_CLIENT_SECRET=<client secret>` in `.env`, the file `compose.yaml` passes to the container (`env_file: .env`). `secrets/secrets.env` is only where `init` writes its three generated values for you to copy into `.env`; the server never reads it. The server discovers the provider's endpoints from `<OIDC_ISSUER>/.well-known/openid-configuration` at start-up.
3. Run `docker compose up -d`, which recreates the container with the new `.env`. Complete a claim from the claim URL (§3), and run the claim checks in the §5 preflight.

**Web search is optional.** Create your own Brave Search API key at <https://brave.com/search/api/> and set `BRAVE_SEARCH_API_KEY=<your-brave-key>`; without it the web-search tool is off.

An animated architecture walkthrough and the full soak report exist outside this repository; neither is needed to bring up Pip.

### Confirm on your Pip

robot-buddy's code fixes what it expects; your butty build fixes what is fitted. Where they could differ, check your robot before you rely on it. Each item says how. Numbers in brackets in the hardware table point here.

1. **Pi model and RAM.** The spec part is a Raspberry Pi 5, **8 GB** (butty `SPEC.md`). The robot service peaked at 185.9 MiB (process) and 207.6 MiB (with sampled children) over the 30-minute soak, and its systemd budget is 1 GiB. butty's own docs also mention a 4 GB Pi 5; that fits the same budget. Check: `free -h`.
2. **Microphone.** The spec part is a Seeed **reSpeaker Flex (XVF3800)**, USB (butty `SPEC.md`, `cad/params.py`). butty `brain/config.yaml` gives its layout: channel 0 is the processed beam, channel 1 the ASR beam. robot-buddy captures 16 kHz S16_LE through `arecord` with 1 to 8 channels and keeps the one you pick (`apps/robot/src/listen`). Check with the service stopped: `arecord -L`, then the capture command in §4. Set `BUDDY_LISTEN_DEVICE`, `BUDDY_LISTEN_CHANNELS=2` and `BUDDY_LISTEN_CHANNEL=0`. butty's docs also mention a reSpeaker Lite (`brain/README.md`); it works with robot-buddy if it captures at 16 kHz S16_LE, with `BUDDY_LISTEN_CHANNELS` and `BUDDY_LISTEN_CHANNEL` set to its own layout.
3. **Operating system.** **Default:** Raspberry Pi OS (64-bit) **Bookworm**, as in Brittian's spec sheet and butty `brain/README.md`, with the native full build on the Pi (§2). That binary passed the ARM64 matrix but not the soak. **Alternative:** Trixie and the soaked ELF (§2), which is the exact binary that passed the soak but moves off the spec's OS. The soaked ELF does not run on Bookworm. Check: `cat /etc/os-release`.
4. **Neck servo voltage, IDs and limits.** The STS3215 is sold in a 7.4 V and a 12 V variant, and Pip's neck runs at 12 V. Before you put 12 V on the adapter, read each servo's label or model number and confirm it is the 12 V variant. Once powered, butty's servo tool `info <id>` prints the supply voltage the servo sees (neck bootstrap step 0 in §4); that confirms the supply, not the variant, so it does not replace the label check. robot-buddy has no default calibration: you write a per-robot `neck.json` in raw steps (§4). butty `brain/config.yaml` uses IDs yaw `1`, pitch `2`, roll `3` at 1 Mbaud (yaw and pitch inverted, roll centre `2107`); butty's yaw centre shift (about 228 steps) lives in an untracked `config.local.yaml`. Check: `robot-buddy-robot servo ping`, `read` and `verify` in §4 (it has no ID-assignment command; §4 says how to set IDs); measure your own limits, never copy another robot's.
5. **Serial paths.** robot-buddy does not auto-detect. Use the two distinct names under `/dev/serial/by-id/` (face board vendor `303a`, servo adapter `1a86`), or butty's udev names `/dev/pip-face` and `/dev/pip-servos` if that rule is installed. Check: `ls -l /dev/serial/by-id/`.
6. **Audio card names.** Use what your Pi reports, not an example. Check: `aplay -L` (the MAX98357A appears after the overlay in §3) and `arecord -L`. Put the exact names in `BUDDY_ALSA_DEVICE` and `BUDDY_LISTEN_DEVICE`.
7. **Camera orientation and tuning.** robot-buddy captures 384 × 288 at 15 fps. butty uses the `ov5647_noir.json` tuning file with `mirror_x`. Check: `rpicam-hello --list-cameras`, then the face panel preview in §4; set `BUDDY_VISION_TUNING_FILE` (optional) and `BUDDY_VISION_ROTATE_180=1` if the image is upside down.
8. **Face firmware.** robot-buddy's face link (115200 baud, native USB CDC, one JSON object per line, `apps/robot/src/face.rs`) matches butty firmware on `main` at `c0ca680` (last firmware commit `b2fc552`). Check: the face heartbeat and eye test in §4. If eyes or ears misbehave, reflash from that revision (§4).
9. **Ear trims and pins.** butty `firmware/include/pins.h` (table below) and its ear trims were set for one build. Check: both ears centre and move without binding after flashing; adjust the trims in your butty checkout if not.
10. **Speaker overlay.** Use `dtparam=audio=off` and `dtoverlay=max98357a` with the amp's SD pin tied to 3.3 V (butty `brain/README.md`, §3). Check with the service stopped: `aplay -D "$BUDDY_ALSA_DEVICE" <any 16-bit WAV>` plays on the 3 W speaker.
11. **Offline voice (optional).** Spoken replies come from the server. Piper is used only for optional offline announcements, and robot-buddy names no voice: `BUDDY_PIPER_MODEL` takes any Piper `.onnx` with its `.onnx.json`. butty uses `en_US-amy-medium` (`brain/config.yaml`). Skip Piper for first bring-up.

The `hey_pip.onnx` from butty `c0ca680` (SHA-256 pinned below) is the same file the 30-minute soak used (`soak-30m-c5d7c08` report, `provenance.files.heyPipModel.sha256`).

If your own rebuild of the soaked recipe gives a SHA-256 other than the one in §2, do not install it as the soaked binary: it is unsoaked. Use it only for bench work, and run the §5 checklist in full.

### Hardware reference

This table is a wiring and verification reference. Sources: Brittian's spec sheet, butty `SPEC.md`, `REV2.md`, `README.md`, `firmware/README.md`, `firmware/include/pins.h`, `brain/config.yaml`, `brain/README.md` and `cad/params.py`, all at `c0ca680`. Numbers in brackets refer to the "Confirm on your Pip" items above.

| Area | Part | Connects to, and the robot-buddy setting |
| --- | --- | --- |
| Main computer | Raspberry Pi 5, 8 GB in the spec [1], official Active Cooler, 64 GB microSD | Raspberry Pi OS 64-bit [3]. Runs `robot-buddy-robot` |
| Face board | ESP32-S3-DevKitC-1 **N16R8** (16 MB QIO flash, 8 MB octal PSRAM) | Pi USB to the board's native **USB** port (GPIO 19 D−, GPIO 20 D+), not UART. USB vendor `303a`. 115200 [8]. `BUDDY_FACE_SERIAL` [5] |
| Eyes | 2 × GC9A01 1.28 in round LCD, 240 × 240, under 30 mm glass domes | One SPI controller (SPI3), with separate clock and data wires per eye [9]. About 21 fps measured at 40 MHz (butty `firmware/README.md`); the spec says ~22 fps |
| Neck | 3 × Feetech **STS3215** 12 V serial bus servos (yaw turns the whole body on a lazy-susan bearing, then pitch and roll) and a USB serial bus servo driver (WCH CH343, vendor `1a86`) | Pi USB, 1 Mbaud, 12 V on the adapter, only after you confirm the 12 V variant (the STS3215 also comes as 7.4 V) [4]. IDs [4]. `BUDDY_SERVO_PORT` [5], `BUDDY_SERVO_CALIBRATION` |
| Ears | 2 × MG90S micro servos | Signal from ESP32 GPIO 4 (left) and 5 (right), 50 Hz. Power from buck B 5 V, never the ESP32 5 V pin |
| Chest light | WS2812B 12-LED ring | Data from ESP32 GPIO 6 through 330 Ω, 5 V from buck B, 470–1000 µF across 5 V/GND. Modes `idle`, `listening` (orange), `thinking`, `speaking`, `muted`; robot-buddy drives the same five |
| Camera | **OV5647** 160° wide-angle CSI, in the visor between the eyes, with a 22→15-pin Pi 5 camera cable | Pi CSI port. `rpicam-vid`, `BUDDY_VISION` [7] |
| Microphone | Seeed **reSpeaker Flex (XVF3800)**, USB [2] | Pi USB, 16 kHz S16_LE, 2 channels: 0 processed beam, 1 ASR beam. `BUDDY_LISTEN_DEVICE`, `BUDDY_LISTEN_CHANNELS=2`, `BUDDY_LISTEN_CHANNEL=0` [6] |
| Speaker | **MAX98357A** I2S amp and a 3 W speaker | Pi GPIO 18 (BCLK), 19 (LRCLK), 21 (DIN), SD tied to 3.3 V. `dtoverlay=max98357a` [10]. `BUDDY_ALSA_DEVICE` [6] |
| Power | 12 V 5 A supply into one 5.5 × 2.1 mm barrel jack in the base; 2 × 12 V→5 V 5 A **plain** USB-C buck converters (not USB-PD) | Wiring below. Keep a 12 V cut-off within reach |
| Structure | Parametric CAD in Python (build123d), printed on a Bambu P1S | Matte PLA shell, PETG neck yokes and eye pods |

**Face board pin map** (butty `firmware/include/pins.h` at `c0ca680`):

| Function | ESP32-S3 GPIO |
| --- | --- |
| LCD SCLK / MOSI, left eye | 12 / 11 |
| LCD SCLK / MOSI, right eye | 17 / 18 |
| LCD DC, RST (both eyes) | 9, 10 |
| LCD CS, left / right | 13 / 14 |
| LCD backlight (both, PWM) | 21 |
| Ear servo, left / right | 4 / 5 |
| LED ring data | 6 |

"Left" is the eye on the viewer's left when facing Pip. The pins avoid the strapping pins, native USB (19, 20), flash and PSRAM, the on-board RGB LED and UART0.

**Wiring and power** (butty `firmware/README.md`):

```text
12 V in ──┬── buck A (5 V 5 A) ── Pi 5 USB-C only
          ├── buck B (5 V 5 A) ── MG90S ear servos V+, WS2812B ring 5V
          └── servo bus adapter (12 V) ── 3 × STS3215
Pi USB ── ESP32-S3 native USB (rev 1: a cut USB cable soldered to 5V, GND, GPIO19 D-, GPIO20 D+)
GND: buck A, buck B, ESP32, ring and ear servos all share ground
Servo ground jumper: ESP32 top-right GND pin ── head's ear-servo ground splice (both servo browns + buck B GND)
```

- The servo ground jumper is a short wire that gives the ear servos' signals a short ground reference (butty `firmware/README.md`, "Servo ground jumper").
- Never connect the Pi or ESP32 USB 5 V to buck B. Never power the ear servos from the ESP32's 5 V pin: a stall pulls about 700 mA and resets the board.
- Keep a 12 V cut-off within reach. Software cannot confirm torque-off after a power loss, a broken cable or a killed process.
- Rev-1 fit issues, including the head hitting a stop at about +21.5° pitch-up (butty's software limit is +20°), are in butty `REV2.md`. Measure your own build's limits (§4).

## 1. Bring up the backend and issue one device identity

Reusing Brittian's backend? Skip this section: you already have the identity file and the URLs (§0).

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
- **Discovery availability at startup:** when `OIDC_ISSUER` is set, the server fetches `<OIDC_ISSUER>/.well-known/openid-configuration` once over HTTPS **at boot** with HTTP redirects disabled (20 s timeout). Startup fails if the discovery endpoint is unreachable, redirects, or returns an `issuer` string that does not match `OIDC_ISSUER` verbatim (including any trailing slash). Leave `OIDC_ISSUER` unset to boot without browser claiming; Pip then connects but refuses to talk until it has an owner (§0).
- **Provider capabilities:** the discovery document must publish HTTPS `authorization_endpoint`, `token_endpoint`, `userinfo_endpoint`, and `jwks_uri` (all answering without HTTP redirects). The client must support Authorization Code + PKCE (`S256`), `scope=openid profile`, `RS256` or `ES256` ID tokens carrying `kid` and `nonce`, and a `userinfo_endpoint` returning the same `sub` as the ID token.
- **Browser redirect & error behavior:** multi-origin HTTPS redirects during sign-in (for example, federated SSO or a separate login domain) are permitted by `/claim`'s `form-action 'self' https:` policy; plaintext `http:` hops are not. If the user cancels or the IdP returns `?error=...&state=...`, `/auth/callback` verifies the login cookie and `state`, spends the transaction, clears `buddy_login`, and renders a static retry page without echoing query parameters.

```sh
chmod 0600 .env
docker compose up --build -d
docker compose ps
curl --fail http://127.0.0.1:8080/ready
```

Route the registration origin to `127.0.0.1:8080`, and the Rivet origin (including WebSocket upgrades) to `127.0.0.1:6420` through your TLS proxy ([TLS in front of the server](#tls-in-front-of-the-server)). Do not log claim URLs, request credentials or query strings. [compose.yaml](compose.yaml) mounts a named volume at `/data`. One running engine per volume; stop it for a consistent backup and keep the matching secrets. Use the [backend guide](docs/architecture/backend.md) for restore or S3 setup. The local Compose lab and `--simulate` are development paths, not the hardware deployment.

### TLS in front of the server

Pip talks to the server only over HTTPS and WSS. On the robot, `BUDDY_SERVER_URL` and the Rivet endpoint the server hands back at enrollment must be `https://` or `wss://`; plain HTTP is accepted only to `localhost` or `127.0.0.1` under `--simulate` (`safe_url` in `apps/robot/src/config.rs`). The server likewise refuses a non-HTTPS `BUDDY_PUBLIC_URL` or `BUDDY_PUBLIC_RIVET_URL` unless `BUDDY_ALLOW_INSECURE_LOCAL=true` and the host is loopback (`apps/server/src/config.rs`).

The robot's HTTPS client (reqwest with rustls) and its Rivet WebSocket client (`rivetkit-client` 3.1.1 over `tokio-tungstenite` with rustls) both trust the bundled Mozilla root set (`webpki-roots`) and the Pi's system trust store (`rustls-native-certs`); `rivetkit-client` turns on both sets (`cargo tree -p robot-buddy-robot -e features -i rustls-native-certs`). The owner's phone also opens `BUDDY_PUBLIC_URL` to claim. So:

- **Use a domain you control and certificates from a public CA.** Point two DNS names at the server host, one for `BUDDY_PUBLIC_URL` and one for `BUDDY_PUBLIC_RIVET_URL`. Nothing needs to be installed on the Pi or the phone.
- A private CA also works for the robot if you add it to the Pi's store (copy it into `/usr/local/share/ca-certificates/`, then `update-ca-certificates`; the [deployment rehearsal](tests/pip-deploy-qa/rehearsal.mjs) does this), but every phone used to claim must trust it too.

[compose.yaml](compose.yaml) publishes the server on `127.0.0.1:8080` and the Rivet engine on `127.0.0.1:6420`. A Caddy reverse proxy on the same host is the shortest recipe. Caddy gets public certificates by itself once both names resolve to the host and ports 80 and 443 reach it, and `reverse_proxy` passes WebSocket upgrades through. `/etc/caddy/Caddyfile`, with your own names:

```caddyfile
buddy.example.com {
	reverse_proxy 127.0.0.1:8080
}

rivet.buddy.example.com {
	reverse_proxy 127.0.0.1:6420
}
```

Then set `BUDDY_PUBLIC_URL=https://buddy.example.com` and `BUDDY_PUBLIC_RIVET_URL=https://rivet.buddy.example.com` in `.env`, run `docker compose up -d`, and check `curl --fail https://buddy.example.com/ready` from another machine. Caddy keeps no access log unless you add a `log` directive. Leave it out: claim URLs carry a token in the query string. Caddy sends `X-Forwarded-For`; the server's per-client rate limits use it only from addresses listed in `BUDDY_TRUSTED_PROXY` (the proxy's address as the server sees it, see [.env.example](.env.example)). Left empty, every request through the proxy shares one rate-limit bucket.

## 2. Build the Linux ARM64 binary

**Default: Raspberry Pi OS (64-bit) Bookworm with the native full build.** Brittian's spec sheet and butty `brain/README.md` ("Raspberry Pi OS setup") both name Bookworm (Debian 12), so this path keeps the spec's OS. Build on the Pi itself with the [Bookworm native full build](#bookworm-native-full-build-on-the-pi-default) below. The ARM64 [toolchain matrix](infra/toolchain-validation.md) built and tested this configuration (private OpenCV 4.10 prefix, full Pip features) at `82e8d5b`, but no Bookworm binary has run the 30-minute soak. Its SHA-256 will differ from the soaked one, so record it.

**Alternative: Raspberry Pi OS (64-bit) Trixie with the soaked ELF.** The 30-minute E2E soak (artifact `soak-30m-c5d7c08`; see [CHANGELOG.md](CHANGELOG.md#validation-status)) ran a robot ELF with SHA-256 `be7cae4e42fda047f857d327af875e0dbf91b54af3a8076df84080e802e2768b` and GNU BuildID `c15c88b7594bd0919c5a0bf78609c2b01c6162e9`. It comes from the `build-robot` stage of the [pip-e2e image](tests/pip-e2e/Dockerfile): `rust:1.100.0-slim-trixie`, `--features pi-vision,onnx-wake`, `aarch64-unknown-linux-gnu`. It links Trixie's packaged OpenCV 4.10 (`libopencv_*.so.410`).

**The trade-off.** Trixie gives you the exact binary that passed the soak, byte for byte, but moves the robot off the spec's OS. Bookworm keeps the spec's OS, but its binary is built on your Pi and has passed the matrix, not the soak. Either way, check with `uname -m` → `aarch64`, `dpkg --print-architecture` → `arm64` and `cat /etc/os-release`. The binary needs the OpenCV and glibc ABI it was built against, so do **not** copy a Trixie binary onto Bookworm. Models are installed later, on the device.

To get the soaked ELF (Trixie alternative) and verify it, on a machine with Docker Buildx for `linux/arm64`:

```sh
tests/pip-e2e/run-in-docker.sh build                    # image robot-buddy-pip-e2e:arm64 (override with PIP_E2E_IMAGE)
docker create --name pip-elf robot-buddy-pip-e2e:arm64
docker cp pip-elf:/usr/local/bin/robot-buddy-robot target/pip-e2e-robot-buddy-robot
docker rm pip-elf
sha256sum target/pip-e2e-robot-buddy-robot
# expect be7cae4e42fda047f857d327af875e0dbf91b54af3a8076df84080e802e2768b
file target/pip-e2e-robot-buddy-robot
# expect ELF 64-bit ... ARM aarch64 ... BuildID[sha1]=c15c88b7594bd0919c5a0bf78609c2b01c6162e9
```

Check the SHA-256 again on the Pi after you install it (`sha256sum /usr/local/bin/robot-buddy-robot`), and record it.

**Reproducibility, checked on 2026-11-24.** Clean rebuilds, with fresh Cargo caches and the build stage not cached, each produced `be7cae4e…2768b` with BuildID `c15c88b7…2e9`, byte for byte:

- the pip-e2e `build-robot` stage from the `82e8d5b` sources;
- the same stage from the `c5d7c08` sources. The only robot change between the two, `0f0bc60`, edits test code, so the release binary is identical;
- [`infra/pi-build.Dockerfile`](infra/pi-build.Dockerfile) (`artifact` stage) from the `82e8d5b` sources.

The robot sources at the branch tip are the same as at `82e8d5b`; only docs and bench files changed since. The recipes do not pin the Debian package versions they install (`clang`, `libclang-dev`, `libopencv-dev`), so a later rebuild can differ once Trixie updates those packages. A different hash means the binary is not the soaked one: treat it as unsoaked and run the §5 checklist in full.

Each build path leaves the binary in a different place. §3 installs it from there.

| Build path | Where it runs | Commands | Artifact (relative to the robot-buddy checkout) |
|---|---|---|---|
| **Bookworm, full (default)** | On the Pi | [Bookworm native full build](#bookworm-native-full-build-on-the-pi-default) below | `target/release/robot-buddy-robot`, on the Pi |
| **Trixie, soaked ELF (alternative)** | Any machine with Docker Buildx for `linux/arm64` | E2E image commands above | `target/pip-e2e-robot-buddy-robot`, on that machine |
| Trixie, full, on the Pi | On the Pi | Install `build-essential cmake clang libclang-dev pkg-config libopencv-dev`, then `cargo build -j2 --release --locked -p robot-buddy-robot --features pi-vision,onnx-wake` | `target/release/robot-buddy-robot`, on the Pi |
| Trixie, full, from a workstation | Mac or Linux with Docker Buildx | [Pi build image](#trixie-from-another-workstation-or-a-base-build-without-vision) below | `target/pi-image/robot-buddy-robot`, on that machine |
| Either OS, no camera, on the Pi | On the Pi | `cargo build --release --locked -p robot-buddy-robot` | `target/release/robot-buddy-robot`, on the Pi |
| No camera, cross-compiled | Linux build host | GNU toolchain commands below | `target/aarch64-unknown-linux-gnu/release/robot-buddy-robot`, on that host |

Every build on the Pi needs a robot-buddy checkout there and the pinned Rust toolchain, as below.

### Bookworm: native full build on the Pi (default)

Bookworm's packaged OpenCV 4.6 predates the recommended 2023 YuNet. Build **OpenCV 4.10** beside the OS libraries rather than replacing the distro's camera stack. You also need a robot-buddy checkout on the Pi and Rust **1.100.0**, the toolchain [rust-toolchain.toml](rust-toolchain.toml) pins, installed with rustup. From a regular account on an ARM64 Bookworm Pi (or an equivalent ARM64 Bookworm build host):

```sh
sudo apt-get install build-essential git curl cmake ninja-build clang libclang-dev pkg-config libjpeg-dev zlib1g-dev
# Rust through rustup (https://rustup.rs), then the pinned toolchain from rust-toolchain.toml.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none
. "$HOME/.cargo/env"
git clone --branch feat/pip-hardware <robot-buddy repository URL> robot-buddy
(cd robot-buddy && rustup toolchain install 1.100.0 --profile minimal --component rustfmt,clippy && rustc --version)  # expect rustc 1.100.0
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
# In the robot-buddy checkout; rust-toolchain.toml selects 1.100.0.
cd robot-buddy
cargo build -j2 --release --locked -p robot-buddy-robot --features pi-vision,onnx-wake
ldd target/release/robot-buddy-robot | grep opencv  # paths under /opt/robot-buddy/opencv-4.10/lib
sha256sum target/release/robot-buddy-robot          # record it; it will not match the soaked Trixie ELF
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

Flash **Raspberry Pi OS Lite (64-bit) Bookworm** (the default), or Trixie if you chose the soaked ELF in §2. For the default native build, flash and boot the Pi first, then run the §2 build on it. Enable SSH on a trusted provisioning network. The Wi-Fi interface must support AP mode under NetworkManager. Keep initial access over Ethernet if you can. Run the administrative steps below as root. If the Pi previously ran butty's Python brain, stop its `pip.service` (`systemctl disable --now pip.service`). Two processes must never own the mic, serial ports or camera at the same time.

```sh
apt-get update
apt-get install network-manager polkitd alsa-utils socat ca-certificates
systemctl enable --now NetworkManager
# Camera profile only (`rpicam-apps-lite` avoids desktop preview dependencies on Lite; `rpicam-apps` also works).
apt-get install rpicam-apps-lite
# Trixie prebuilt/Buildx vision binary runtime libraries ONLY (on Bookworm use the /opt/robot-buddy/opencv-4.10 prefix built above):
# apt-get install libopencv-core410 libopencv-imgproc410 libopencv-imgcodecs410 libopencv-dnn410 libopencv-objdetect410
```

**Power supply and `/boot/firmware/config.txt`:** Pip's reference power topology feeds a 12 V 5 A supply into three branches with a **shared ground** across every rail: (1) Buck A (12 V → 5 V 5 A plain USB-C buck, **not** USB-PD) dedicated to the Pi 5 alone (must actually hold 5 A at ~5.1 V); (2) Buck B (12 V → 5 V 5 A) for the MG90S ear servos and WS2812B chest ring (never join the Pi USB 5 V rail to Buck B); and (3) 12 V direct to the STS3215 serial bus servo adapter. Keep a 12 V cut-off within reach: software cannot confirm torque-off after lost power, a broken cable or a killed process.

Because a plain 5 V USB-C buck does not negotiate USB-PD 5 A, the Pi 5 defaults to capping total USB downstream current at 600 mA across the ESP32-S3, reSpeaker, and servo adapter. Edit `/boot/firmware/config.txt` and reboot. These lines are the same as butty `brain/README.md` ("Raspberry Pi OS setup", step 1); if the Pi already ran butty's brain they may be there already ("Confirm on your Pip" item 10):

```ini
# Set ONLY when powering the Pi 5 from a plain (non-USB-PD) supply genuinely rated
# for 5 V / 5 A (such as Buck A above). Omit on an official 27 W USB-PD 5 A supply
# (which negotiates 5 A over PD), and never set on an under-rated 3 A supply.
usb_max_current_enable=1

# MAX98357A I2S amp (BCLK GPIO18, LRCLK GPIO19, DIN GPIO21, SD tied to 3.3 V): disable onboard audio
# so ALSA does not conflict with the I2S overlay. Use only the overlay for your wiring.
dtparam=audio=off
dtoverlay=max98357a

# OV5647 CSI camera: default camera_auto_detect=1 usually finds it. If
# `rpicam-hello --list-cameras` shows nothing, disable auto-detect and select the wired CSI port:
# camera_auto_detect=0
# dtoverlay=ov5647,cam0
```

Stage **only** the binary, this robot's identity, and the files under `infra/` in one directory on the Pi. Copy the identity over SSH from the machine that holds it. Adjust user, hostname and paths:

```sh
# From the machine that holds secrets/pip-01.identity.json:
ssh pi@pip.local 'umask 077; mkdir -p ~/buddy-install/infra'
scp secrets/pip-01.identity.json pi@pip.local:buddy-install/
```

Then add the binary and the `infra/` files from wherever your §2 build path left them:

```sh
# Default, Bookworm native build: everything is already on the Pi. On the Pi:
cp ~/robot-buddy/target/release/robot-buddy-robot ~/buddy-install/
cp ~/robot-buddy/infra/{robot-buddy.service,robot-buddy-nm.rules,robot-buddy-pip.conf,device.env.example} ~/buddy-install/infra/

# Alternative, soaked Trixie ELF: from the machine with the robot-buddy checkout and the ELF.
scp target/pip-e2e-robot-buddy-robot pi@pip.local:buddy-install/robot-buddy-robot
scp infra/{robot-buddy.service,robot-buddy-nm.rules,robot-buddy-pip.conf,device.env.example} pi@pip.local:buddy-install/infra/
```

For the other build paths, copy the artifact named in the §2 table to `buddy-install/robot-buddy-robot` the same way.

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

When no working network is available, join the printed per-device setup SSID with its password, open `http://10.42.0.1` and choose the home network. Do not enable a wait-online dependency: this unit provides setup when offline. Once connected, print the claim URL on the Pi with `runuser -u robot-buddy -- robot-buddy-robot claim-url` and open it on your phone. The claim panel on `127.0.0.1:8090` also shows it, but only on the Pi's loopback; from another computer, tunnel it first with `ssh -N -L 127.0.0.1:8090:127.0.0.1:8090 <user>@<pip-host>` and open `http://127.0.0.1:8090/`. Complete OIDC sign-in and confirm the **matching six digits** on the robot's panel or through speech. Treat setup cards, claim URLs and device journals as private. Preserve the printed card even if the eyes display setup QR codes.

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

* **Face firmware:** this repo does not contain it. Flash the PlatformIO project in butty's `firmware/` directory from butty `main` at `c0ca680`; the last commit there that touched `firmware/` is `b2fc552`. butty has no other firmware branch, and robot-buddy needs no newer or full-duplex firmware. The project uses the Arduino framework, `platform = espressif32@6.9.0`, board `esp32-s3-devkitc-1` with 16 MB QIO flash and 8 MB octal PSRAM, and the default env `pip-face` (LovyanGFX eyes, ArduinoJson commands, Adafruit NeoPixel ring).

  The robot-buddy face link (`apps/robot/src/face.rs`) speaks the same one-JSON-object-per-line protocol at 115200 on the native USB CDC port. Pi → board: `{"e":…,"i":…}`, `{"look":[x,y]}`, `{"led":…}`, `{"g":"blink"|"ear_wiggle"}`, `{"qr":[…],"w":…}` and `{"ping":n}`. Board → Pi: `{"hello":"pip-face",…}` at boot and a `{"hb":…}` heartbeat every second. robot-buddy re-sends the current mood every 1.5 s, inside the board's 5 s window; with no valid line for 5 s the board falls back to a calm idle face. robot-buddy caps a QR line at 560 bytes, which fits the 600-byte line buffer in butty `firmware/src/main.cpp` at `c0ca680` (the protocol table in butty `firmware/README.md` still says "max 159 bytes"). This compatibility comes from reading both sides and from the virtual face in the E2E harness, not from a physical board; see "Confirm on your Pip" item 8.

  To flash it from a Mac or the Pi:

  ```sh
  pipx install platformio           # or: uv tool install platformio. Plain pip install fails on Bookworm (PEP 668)
  cd <butty checkout>/firmware
  pio run                           # build the default pip-face env
  pio run -t upload                 # flash over the board's native USB port
  pio device monitor                # 115200; expect a hello line, then one hb per second; close it afterwards
  ```

  On the Pi, **stop the service first** (`systemctl stop robot-buddy`) so it does not hold the CDC-ACM port open. Use the port labelled **USB** (native USB), not UART. If upload does not enter download mode, enter the ROM bootloader by hand (**hold `BOOT`, tap `RST`, release `BOOT`**), run `pio run -t upload` again, and tap `RST` afterwards if needed. If the eyes stay dark, butty's `firmware/README.md` lists the diagnostic envs (`pin-check`, `bitbang-test`, `screen-test`, `wire-check`, each flashed with `pio run -e <name> -t upload`). Ear centre and direction are compile-time values (`EAR_CENTER_US_L/R`, `EAR_DIR_L/R` in `include/pins.h`), calibrated with `{"e":"neutral","i":1}` and `{"e":"surprised","i":1}` from the monitor.
* **Face on the Pi:** pick the board's path from `ls -l /dev/serial/by-id/` (Espressif USB vendor `303a`; butty's docs call it `/dev/ttyACM0`, "Confirm on your Pip" item 5) and set `BUDDY_FACE_SERIAL` to it. The client sets raw 115200 on every open. Check both eyes, the ring, ear movement and the heartbeat. Then unplug and replug the face and confirm it recovers.
* **Neck:** the CH343/STS3215 adapter has a **different** serial path from the face board. If an adapter lacks a unique ID, use a stable USB topology path under `/dev/serial/by-path/`. Every `robot-buddy-robot servo` command, even `ping` and `read`, loads a valid `neck.json` first (`apps/robot/src/motion/mod.rs`), and factory STS3215s all answer on ID 1. So bring the neck up in the order below. Do not copy calibration between robots.

**Neck envelope.** `neck.json` is refused unless every joint fits these ceilings (`apps/robot/src/motion/calibration.rs`). One step is 360/4096 degrees.

| Joint | Max travel from `center`, each side | Max `max_speed_deg_s` | `accel` | butty `brain/config.yaml` |
| --- | --- | --- | --- | --- |
| `yaw` | 1024 steps (90°) | 120 | 1–254 | ±90°, 120 °/s |
| `pitch` | 228 steps (about 20°) | 90 | 1–254 | ±20°, 90 °/s |
| `roll` | 285 steps (about 25°) | 90 | 1–254 | ±25°, 90 °/s |

Also: `max_speed_deg_s` at least 1; IDs 1–253 and all different; `min_steps < center < max_steps <= 4095`; no unknown fields; file at most 8 KiB. A wider range fails with `travel exceeds neck envelope for ID <n>`.

**Neck bootstrap, in order.** Stop the service first (`systemctl stop robot-buddy`) so nothing else holds the adapter, and support the head. robot-buddy has no ID or voltage command, so steps 0 and 1 use butty's servo tool (`brain/pipbrain/tools/servo_tool.py`). It talks 1 Mbaud, takes the adapter with `--port` **before** the subcommand, and reads butty's `brain/config.yaml` at start, so run it from butty's `brain/` directory. A base butty virtualenv is enough: the base dependencies in butty `brain/pyproject.toml` include `pyserial` and `pyyaml`, and the tool needs none of the `voice`, `vision` or `sim` extras. Run it as an account in the `dialout` group:

```sh
sudo apt install python3-venv        # butty brain/README.md installs it too; skip if python3 -m venv already works
cd <butty checkout>/brain
python3 -m venv .venv && .venv/bin/pip install -e .
PORT=/dev/serial/by-id/YOUR_SERVO_ADAPTER
.venv/bin/python -m pipbrain.tools.servo_tool --port "$PORT" scan      # pings IDs 1 to 20
.venv/bin/python -m pipbrain.tools.servo_tool --port "$PORT" info 2    # one ID: supply voltage, temperature, position, EEPROM limits
```

0. **Check the voltage, then scan with everything connected.** Before 12 V goes on the adapter, confirm from each servo's label or model number that it is the 12 V STS3215, not the 7.4 V one ("Confirm on your Pip" item 4). Then power the adapter, leave all three servos connected as assembled, and run `scan`. It prints `servo IDs found: [...]`.
   - **`[1, 2, 3]`:** the IDs are already set, as they usually are on an assembled Pip. **Skip step 1.** Run `info 1`, `info 2` and `info 3`; each prints `supply <n> V`, which should read close to your 12 V supply. If it does not, cut 12 V and check the wiring. This confirms the supply the servo sees, not its variant, so it does not replace the label check.
   - **Only `[1]`, `none`, or IDs you do not expect:** some servos are unknown or still on the factory ID 1. Several servos on one ID answer over each other, so the scan cannot tell how many there are. Go to step 1 for those servos only.
   - Without butty's tool, write the provisional `neck.json` from step 2 first and run `robot-buddy-robot servo ping` (block below). It prints `id <n>: ok` per ID and stops with an error at the first ID that does not answer. That is enough when the IDs are already 1, 2 and 3, but it cannot read the voltage or set an ID.

1. **Only for unknown or factory (all ID 1) servos: set IDs one servo on the bus at a time.** Disconnect every other servo from the bus, so the one you are setting is the only one connected, then:

   ```sh
   .venv/bin/python -m pipbrain.tools.servo_tool --port "$PORT" scan       # expect [1] for a factory servo
   .venv/bin/python -m pipbrain.tools.servo_tool --port "$PORT" set-id 1 2   # pitch servo alone on the bus
   .venv/bin/python -m pipbrain.tools.servo_tool --port "$PORT" set-id 1 3   # roll servo alone on the bus
   ```

   > **Never run `set-id` with more than one servo on the bus.** It writes the new ID to whatever answers on the old one, with no check (butty `servo_tool.py`). On an assembled Pip with IDs 1, 2 and 3, `set-id 1 2` renames yaw to 2, and yaw then collides with pitch.

   Yaw keeps ID 1. This is butty's convention, yaw 1, pitch 2, roll 3 ("Confirm on your Pip" item 4); butty `brain/README.md` §6 does it "once, before assembly". Reconnect all three and `scan` again: expect `[1, 2, 3]`. The Feetech vendor tool also sets IDs.
2. **Write a provisional, narrow `neck.json`** as a root-owned `0640 root:robot-buddy` file at `/etc/robot-buddy/neck.json`. It only lets `ping` and `read` run; `read` prints raw positions even outside these limits. Example shape, inside the envelope, **not** a calibration:

   ```json
   {
     "yaw":   {"id": 1, "center": 2048, "min_steps": 2000, "max_steps": 2096, "invert": false, "max_speed_deg_s": 10, "accel": 5},
     "pitch": {"id": 2, "center": 2048, "min_steps": 2000, "max_steps": 2096, "invert": false, "max_speed_deg_s": 10, "accel": 5},
     "roll":  {"id": 3, "center": 2048, "min_steps": 2000, "max_steps": 2096, "invert": false, "max_speed_deg_s": 10, "accel": 5}
   }
   ```

3. **Ping, then read raw centres.** With all three servos on the bus, run `servo ping`, which prints `id <n>: ok` for each. Pose the head straight ahead by hand and run `servo read`, which prints `id <n>: <steps> steps`. Those are your centres. If a joint holds torque and resists, run `servo verify` once: it turns torque off on all three before it checks anything (and fails at this stage, which is expected). Then move each joint by hand to its cable-safe limits and `read` again. Move one joint at a time: the ID whose steps change is that joint, which confirms the yaw, pitch and roll IDs. While you do it, note for each joint whether its steps **rise or fall** when you move it the positive way in step 4's convention.
4. **Write the real limits** in `neck.json`: measured `center`, `min_steps` and `max_steps` inside the cable-safe range and the envelope above, `invert` per joint by the convention below, and a conservative `max_speed_deg_s` and `accel`.

   **Sign convention.** robot-buddy plans in degrees from `center`: degrees = (steps − `center`) × 360/4096, negated when `invert` is `true`, and commands convert back the same way (`Joint::degrees` and `Joint::steps` in `apps/robot/src/motion/calibration.rs`). So with `invert: false`, positive degrees are rising raw steps. No robot-buddy comment states which way positive is, but the motion code fixes it, and it is the same as butty's (butty `brain/pipbrain/motion.py`). "Viewer" is someone standing in front of Pip, facing it:

   | Joint | Positive means | Where robot-buddy fixes it |
   | --- | --- | --- |
   | `yaw` | Pip turns toward the viewer's right (Pip's own left) | Face tracking: yaw error = −60 × x, where x is the face's position in the camera image, right positive (`Planner::step` in `apps/robot/src/motion/poses.rs`; `Observation` in `apps/robot/src/vision/mod.rs`). An outward camera sees the viewer's right on the image's left, so x < 0 and yaw goes positive; the eyes get −x for the same reason (`apps/robot/src/hardware.rs`, the vision callback). butty maps the image the same way |
   | `pitch` | The head looks up | Tracking: pitch error = −45 × y, image y down positive, so a face above centre raises pitch. Poses: `asleep` −19.5°, `sad` −12°, `surprised` +12° (`expression` in `poses.rs`) |
   | `roll` | The head tilts toward the viewer's right | Only the poses, because nothing observes roll: gesture `tilt` +18°, `curious` +13°, `asleep` −6° (`poses.rs`). butty uses the same values with this convention |

   Two things the code cannot settle for you. First, which way raw steps run depends on how each servo is mounted, so set `invert: false` for a joint whose steps rose when you moved it the positive way in step 3, and `invert: true` if they fell. butty's `brain/config.yaml` inverts yaw and pitch for butty's build; treat that as a hint, not proof for yours. Second, the yaw and pitch tracking directions also depend on the camera image being upright; step 8 separates a wrong `invert` from an upside-down camera.
5. **Provision EEPROM:** `servo provision … --write-eeprom`. It needs every servo already in position mode and each joint physically inside its new range. It writes the limits, relocks EEPROM, then verifies.
6. **Verify:** `servo verify` checks position mode, the EEPROM lock and limits against `neck.json`, and each joint's position, and leaves torque off.
7. **Centre, under supervision:** `servo center` arms the neck, moves it to `center` at your speed limit, and turns torque off when it finishes or fails.

```sh
systemctl stop robot-buddy
PORT=/dev/serial/by-id/YOUR_SERVO_ADAPTER
CAL=/etc/robot-buddy/neck.json
runuser -u robot-buddy -- robot-buddy-robot servo ping "$PORT" "$CAL"
runuser -u robot-buddy -- robot-buddy-robot servo read "$PORT" "$CAL"
# After writing the measured limits (step 4):
runuser -u robot-buddy -- robot-buddy-robot servo provision "$PORT" "$CAL" --write-eeprom
runuser -u robot-buddy -- robot-buddy-robot servo verify "$PORT" "$CAL"
runuser -u robot-buddy -- robot-buddy-robot servo center "$PORT" "$CAL"
```

Never widen limits to make a failed verification pass.

8. **Check each direction, before face tracking.** Set `BUDDY_SERVO_PORT` and `BUDDY_SERVO_CALIBRATION` in `device.env`, leave `BUDDY_VISION` unset so tracking cannot move the neck yet, start the service and stand in front of Pip with the 12 V cut-off in reach. An idle Pip lowers its head into a resting pose after a while, so send `{"wake":true}` first to start from neutral. Expected motion, in the viewer's frame:

   | Joint | Claim-free check | Expect |
   | --- | --- | --- |
   | `pitch` | As root, `printf '{"sleep":true}\n' \| socat - UNIX-CONNECT:/run/robot-buddy/control.sock`. This holds the `asleep` pose, pitch −19.5° and roll −6°, clamped 8 steps inside your limits (`Control::Sleep` in `apps/robot/src/controller.rs`). `printf '{"wake":true}\n'` the same way returns to neutral | The head looks **down** |
   | `roll` | The same `sleep` command | The head tilts a little toward the **viewer's left** |
   | `yaw` | No claim-free pose moves yaw: the control socket has no pose or gesture command (`apps/robot/src/input.rs`). Use the operator panel below, or face tracking once the camera is on | — |

   With a claim, open the operator panel (§4 camera) and use **Hold pose** with one slider at +10 and the others at 0, then **Release**. The panel holds a pose for 5 s and clamps it to your limits. Expect: yaw +10, Pip turns toward the **viewer's right**; pitch +10, it looks **up**; roll +10, the head tilts toward the **viewer's right**.

   Without a claim, check yaw with tracking after you enable the camera: stand well to the viewer's right of centre (tracking ignores less than 6° of error) and expect Pip to turn toward you; then crouch below the camera and expect it to look down. If pitch passed the `sleep` check but tracking also moves pitch the wrong way, the image is upside down: set `BUDDY_VISION_ROTATE_180=1`, do not flip `invert`. If only yaw tracks the wrong way, flip yaw's `invert`.

   For any joint that moves the wrong way: stop the service, flip that joint's `invert` in `neck.json`, run `servo verify` again and restart. `invert` changes no EEPROM limit, so no new `provision` is needed.

After step 8, run `systemctl stop robot-buddy` and confirm the head can relax. Remove `BUDDY_SERVO_PORT` and `BUDDY_SERVO_CALIBRATION` to run without the neck. Consult the firmware's own assembly documentation for wiring and ear calibration.

### Microphone, transcription and speaker

The integrated path uses local `arecord` → voice activity detection → `whisper-cli` → transcript wake matching. It does **not** need Python or the `hey_pip.onnx` wake model. Use `arecord -l`/`arecord -L` for the reSpeaker and `aplay -L` for the MAX98357A I2S amplifier (the `dtoverlay=max98357a` card from §3). Which reSpeaker is fitted is "Confirm on your Pip" item 2. This guide assumes a **2-channel** 16 kHz S16_LE capture stream, as butty `brain/config.yaml` describes for the XVF3800: open it at its native 2-channel layout (`-c2`, `BUDDY_LISTEN_CHANNELS=2`) and select the processed beam (`BUDDY_LISTEN_CHANNEL=0`, or `1` if your firmware exposes the ASR beam on channel 1) so `plughw:` does not downmix both beams together. Verify capture as `robot-buddy`, with the service stopped, e.g. `runuser -u robot-buddy -- arecord -D 'plughw:CARD=YOUR_MIC,DEV=0' -r16000 -fS16_LE -c2 -d2 -t raw /dev/null` (adjust `-c` and `BUDDY_LISTEN_CHANNELS` only if your microphone's native channel count differs).

Voice activity detection is built into the client (`apps/robot/src/listen/vad.rs`, energy-based, 20 ms frames, clip ends after 800 ms of quiet). There is no VAD model to install. Install [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on the Pi. The harness validated **v1.9.6** (source tarball SHA-256 in the table below), built the way [`install-speech-stack.sh`](tests/pip-e2e/speech/install-speech-stack.sh) does it: `cmake -S . -B build -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF -DWHISPER_BUILD_EXAMPLES=ON -DWHISPER_BUILD_TESTS=OFF -DWHISPER_SDL2=OFF -DGGML_NATIVE=OFF -DGGML_OPENMP=ON`, then `cmake --build build -j2 --target whisper-cli` and `install -m755 build/bin/whisper-cli /usr/local/bin/`. The client passes `-of /dev/null`, so whisper.cpp 1.8 or newer writes the transcript to stdout. Select a local, license-reviewed English ggml model, e.g. `ggml-tiny.en.bin`. For every model in this guide, verify the pinned revision and publisher checksum **before installation**; keep the license/model card with the device inventory. The service does not download weights on first boot. Install models read-only by the service under `/opt/robot-buddy/models`, outside `/home`.

```ini
BUDDY_LISTEN_MODEL=/opt/robot-buddy/models/ggml-tiny.en.bin
BUDDY_LISTEN_WHISPER=/usr/local/bin/whisper-cli
BUDDY_LISTEN_DEVICE=plughw:CARD=YOUR_MIC,DEV=0
BUDDY_LISTEN_CHANNELS=2
BUDDY_LISTEN_CHANNEL=0
```

`CHANNELS` is the captured interleaved channel count, `CHANNEL` the zero-based beam index. Pip answers only once claimed (§0); before that, use the claim-free wake check in [What needs a claim](#what-needs-a-claim). With transcript wake, say “Hey Pip, what time is it?” and test a follow-up after the reply. **Spoken barge-in requires neural wake:** transcript mode does not transcribe playback echo to search for “Hey Pip”, even when `BUDDY_LISTEN_BARGE_IN=1`. With neural wake enabled, test an interruption during speech; set `BUDDY_LISTEN_BARGE_IN=0` if the beam/amp routing does not reject echo. The external socket still works without this listener: put only the external STT account in `pip-control` (not `robot-buddy`). Test as that account: `printf '{"wake":true}\n' | socat - UNIX-CONNECT:/run/robot-buddy/control.sock`. Do not enable two capture processes on the same microphone.

**Optional neural wake:** the full image includes the `onnx-wake` Cargo feature. Install an AArch64 Linux CPU build of [ONNX Runtime](https://github.com/microsoft/onnxruntime/releases). The validated build is **1.20.1**: the `runtimes/linux-arm64/native/` libraries from the `Microsoft.ML.OnnxRuntime` 1.20.1 NuGet package, which match the `ort =2.0.0-rc.9` crate's `ORT_API_VERSION=20`. Checksums are in the table below. Point `ORT_DYLIB_PATH` at the actual `libonnxruntime.so` file, not a directory, and keep `libonnxruntime_providers_shared.so` beside it. Use a local directory containing `hey_pip.onnx` from your butty checkout (`brain/models/wake/hey_pip.onnx`) plus the matching public openWakeWord `melspectrogram.onnx` and `embedding_model.onnx` frontend (`v0.5.1`). Check each file's SHA-256 against the table below; for `hey_pip.onnx` the pin is the file as of butty `c0ca680`, and a later butty commit may change it. Check each model's terms, especially for commercial use. No weights are supplied in this repository. Set e.g. `BUDDY_LISTEN_WAKE_DIR=/opt/robot-buddy/models/wake` alongside `BUDDY_LISTEN_MODEL`. This changes wake detection, not speech transcription. Validate quiet, noisy, positive and negative recorded clips against the reference before enabling it unattended; remove `BUDDY_LISTEN_WAKE_DIR` to return to transcript wake.

#### Verified model pins

Check every file's SHA-256 (`sha256sum`) before you copy it to `/opt/robot-buddy/models` or install it. Fetch the real LFS bytes, not the text pointers. `hey_pip.onnx` is the only model file in butty at `c0ca680`; its pin was computed from that commit. The public pins match [`tests/pip-e2e/speech/assets.lock.json`](tests/pip-e2e/speech/assets.lock.json) and [`fetch-vision-models.sh`](tests/pip-e2e/peripherals/fetch-vision-models.sh), which is what the E2E harness and soak used.

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
| `hey_pip.onnx` | butty `brain/models/wake/hey_pip.onnx`, butty's custom-trained openWakeWord classifier, **as of butty `c0ca680`** (not in this repo; see [From butty](#from-butty)) | 415,225 | `5986afeaa8c7d406aa19dd871703ba792ca611e689676dc37273a5ffb80164ee` |
| Piper voice `.onnx` + `.onnx.json` | Not pinned. butty uses `en_US-amy-medium` (`brain/config.yaml`); no voice files are in butty at `c0ca680` | | None. Record the checksum of the voice you install |

The wake directory (`BUDDY_LISTEN_WAKE_DIR`) must contain these three file names: `melspectrogram.onnx`, `embedding_model.onnx` and `hey_pip.onnx`.

For a repeatable wake check on a private test machine with the matching native ONNX Runtime installed, set `ORT_DYLIB_PATH`, `BUDDY_LISTEN_WAKE_DIR`, `BUDDY_WAKE_TEST_PCM` (16 kHz mono S16_LE, 1280 samples per hop) and `BUDDY_WAKE_TEST_SCORES` (one numeric reference score per hop). Include both classes after the four-second warmup. Run `cargo test --locked -p robot-buddy-robot --features onnx-wake compare_reference_scores -- --ignored`. No clips, reference scores or models should be added to the repo.

**Optional offline voice:** install [Piper](https://github.com/OHF-Voice/piper1-gpl) separately (e.g. in a Pi-compatible, root-owned venv under `/opt/robot-buddy/piper`, with a reviewed pinned `piper-tts` version). It must accept `--model`, `--output-raw`, `--length-scale` and emit signed 16-bit mono PCM on stdout. Install the chosen `.onnx` **and its matching `.onnx.json`**; the latter gives the sample rate. Which voice to choose is "Confirm on your Pip" item 11. robot-buddy applies butty's voice coloring itself (a slower `--length-scale` played back 1.3× faster), so install the plain voice, not butty's `pip_voice.py` output. Use the voice's own model license. No Piper or espeak code is linked into the client.

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

The gallery belongs in the service's `0700` state directory. Use the operator panel to enroll or forget local embeddings. Frame data and recognition are not a cloud identity or authority to confirm claims. To enable the panel, put a unique `BUDDY_OPERATOR_TOKEN` (32–128 printable non-space ASCII characters, e.g. `openssl rand -hex 32`) into the root-only env file, **not** an SSH command or a URL. Restart the service. The panel works only once the robot is connected and claimed ([What needs a claim](#what-needs-a-claim) lists the claim-free camera checks):

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

### What needs a claim

A claim needs OIDC on the server (§0). Until the robot is claimed, the operator panel returns `403 Robot must be connected and claimed.` and Pip refuses utterances. Checks that need neither run without a claim. Where a claim-free check exists it is listed; it covers part of the check, not all of it.

| Check | Needs a claim because | Claim-free check |
| --- | --- | --- |
| Talking: "Hey Pip", a reply and a follow-up (§4 microphone; gate 3 wake, echo and latency) | Pip refuses utterances until it has an owner | Capture: the `arecord` test as `robot-buddy` (§4). Wake and transcription: with the service running and connected, say "Hey Pip, hello". A journal line `utterance refused` with `reason="robot not owned"` means capture, endpointing, `whisper-cli` and wake matching all worked; Pip emits an utterance only after a wake. Speaker: the `aplay` test ("Confirm on your Pip" item 10). Replies, barge-in and latency have no claim-free check |
| Small manual neck pose on the operator panel (§4 neck; gate 2) | Operator panel | With the service stopped and the head supported, `servo center` arms the neck, moves it to `center` and turns torque off when it exits (§4). Then start the service with the neck enabled, look for `neck loop timing` in the journal, and check pitch and roll direction with the control-socket `sleep` pose (§4 neck bootstrap step 8). Stop it and feel all three axes by hand. Yaw direction has no claim-free check except face tracking |
| Camera preview and live events on the operator panel (§4 camera; gate 5) | Operator panel | `rpicam-hello --list-cameras` and the 165888-byte `rpicam-vid` frame check as `robot-buddy` (§4). With `BUDDY_VISION=1` and the service running, `camera inference active` in the journal. Tracking direction: watch the eyes and neck follow a face. There is no claim-free image preview |
| Gallery enroll and Forget (gate 5) | Operator panel | None |
| Operator panel auth (gate 5) | `403` for a claimed robot from a wrong origin | Wrong or missing password returns `401` before the claim check, and the right password on an unclaimed robot returns the `403` above |
| Claim, 6-digit confirmation and cancelled sign-in (gate 1) | This is the claim | None |
| Ownership kept across reboot, upgrade and rollback (gates 1 and 6) | Needs an owner to keep | The bot ID survives without a claim; ownership cannot be checked |
| Loaded soak (gate 4) | The load includes speech | Vision and 50 Hz neck load run without a claim; the speech part does not |

Face (`face hello`, `face heartbeat`), `servo ping`, `read`, `verify`, Wi-Fi setup, enrollment and `/ready` need no claim.

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
| `travel exceeds neck envelope for ID <n>` | A joint in `neck.json` reaches further from `center` than the [neck envelope](#face-and-neck) allows (yaw 1024, pitch 228, roll 285 steps each side). Bring `min_steps` and `max_steps` inside it; the ceilings are fixed in code. `invalid speed or acceleration for ID <n>` is the same check for speed and `accel` |
| `servo <n> at <pos>, outside calibrated range` | The joint sits outside `min_steps`..`max_steps`. With torque off, move it inside by hand, then run `provision`, `verify` or `center` again |
| Camera absent | `rpicam-hello --list-cameras` as `robot-buddy`. `video` group on both the user and `20-pip.conf`. `camera_auto_detect=0` + `dtoverlay=ov5647,cam0` if needed |
| Operator panel `401` / `403` | `401`: missing or wrong credentials. `403` after valid credentials: the bot must be connected and claimed, and the request must come from the expected origin. The panel only binds loopback, so use an SSH tunnel |
| Restarts or OOM | `MemoryPeak` against the 1 GiB drop-in, `vcgencmd get_throttled` (under-voltage means buck A is not holding 5 A at about 5.1 V) |

## Upgrade and rollback

**Before any upgrade**, save the working binary and its SHA-256, the root-only env file, the unit and drop-ins, the polkit rule and `neck.json`. Record the bot ID, ownership, calibration revision and gallery state.

**Upgrade:** stop the service. Replace the binary and any changed unit or rules. Install `40-robot-buddy-nm.rules` after checking site polkit policy, and remove any legacy `/etc/polkit-1/rules.d/50-robot-buddy-nm.rules`. Check the new binary's dependencies (`ldd`) and the unit (`systemd-analyze verify`), then run `systemctl daemon-reload` and restart. Keep `/etc/robot-buddy/device.identity.json`, `/var/lib/robot-buddy/` (cache, setup card, gallery) and the server's volume and secrets. Do not re-run `init` or `provision`, and do not overwrite the env file, during an upgrade.

**Rollback:** stop the unit. Restore the saved binary and its matching configuration, unit, polkit rule and calibration. Then reload and restart. If EEPROM limits changed, reconcile them **with the motors powered, under operator control**, and run `servo verify` before rearming. A device rollback does not roll back server state. The server rolls back separately: stop the container, restore the `/data` volume backup together with its matching secrets, and start it again. The [rehearsal evidence](tests/pip-deploy-qa/EVIDENCE.md) shows a cross-version upgrade and rollback that kept identity and state.
