# Robot Buddy

Rust robot-assistant backend using Rig for model/tool orchestration and Rivet actors for durable state. OpenRouter is the default; an explicitly configured OpenAI-compatible local endpoint is supported. Turborepo coordinates Cargo tasks.

## Run the server

```sh
bun install
bun run check
bun run test
cargo run -p robot-buddy-server -- init secrets
cp .env.example .env
```

Copy the three generated values from `secrets/secrets.env` into `.env`. Set `AI_MODEL`, `AI_API_KEY`, public HTTPS URLs, and OIDC sign-in settings. For a local model, set **both** `AI_PROVIDER=openai_compatible` and `AI_BASE_URL` (for example `http://host.docker.internal:11434/v1` on Docker Desktop). Plain HTTP to a host other than loopback also needs `AI_ALLOW_PLAINTEXT_MODEL=1`, so that example needs it; set it only for a model on a trusted private network, or startup fails. The image talks to its own engine over loopback; if you point `RIVET_ENDPOINT` at an engine elsewhere, it must be HTTPS, or plain HTTP to the one host named by `RIVET_ENDPOINT_PLAINTEXT_HOST` (a private Docker-network service name). Do not commit secrets. The offline `issuer.key` provisions device credentials and is not mounted into the server.

```sh
docker compose up --build -d
```

One container runs the Rust server and pinned Rivet engine. `/data` holds both engine logs and object data. Put your TLS reverse proxy in front of ports 8080 (registration) and 6420 (Rivet HTTP/WebSocket). Compose binds these to localhost. The reverse proxy must preserve WebSockets and must not log credentials/query strings. The image does not include a TLS certificate manager or an identity provider.

Volume-only storage supports a single engine. Rivet recommends external object storage for production; optional S3 configuration is documented in `.env.example`. Never share this volume between running engines. Stop the container before copying the complete volume for backup, retain server secrets separately, and restore both together. See [storage and recovery](docs/architecture/backend.md).

## Provision a robot

```sh
cargo run -p robot-buddy-server -- provision secrets/issuer.key secrets/device.identity.json
cargo build --release -p robot-buddy-robot
```

Install the robot binary on Linux with NetworkManager and a Wi-Fi adapter supporting AP mode. Set `BUDDY_SERVER_URL`, `BUDDY_IDENTITY_FILE`, `BUDDY_CACHE_FILE`, and optionally `BUDDY_WIFI_INTERFACE` (default `wlan0`). The example [systemd unit](infra/robot-buddy.service) runs as the unprivileged `robot-buddy` account; [polkit rules](infra/robot-buddy-nm.rules) let it manage its NetworkManager profiles. Provision each robot a unique identity file. As root:

```sh
groupadd --system pip-control
useradd --system --home-dir /var/lib/robot-buddy --shell /usr/sbin/nologin \
  --groups audio,dialout,pip-control robot-buddy
install -m 0755 robot-buddy-robot /usr/local/bin/robot-buddy-robot
install -d -m 0750 -o root -g robot-buddy /etc/robot-buddy
install -m 0600 -o robot-buddy -g robot-buddy device.identity.json /etc/robot-buddy/
install -m 0600 infra/device.env.example /etc/robot-buddy/device.env   # then edit it
# Priority 40 precedes Debian/RPi OS 49-polkit-pkla-compat.rules. Inspect existing
# /etc/polkit-1/{rules.d,localauthority} first so this does not overturn a stricter site policy.
install -m 0644 infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/40-robot-buddy-nm.rules
install -m 0644 infra/robot-buddy.service /etc/systemd/system/
install -d -m 0700 -o robot-buddy -g robot-buddy /var/lib/robot-buddy
runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot setup-card
systemctl daemon-reload && systemctl enable --now robot-buddy
```

The unit starts beside NetworkManager and does not wait for the network to come up. Do not enable `NetworkManager-wait-online.service` for it.

`setup-card` prints this robot's setup Wi-Fi name, password and a QR code, created once and kept in `/var/lib/robot-buddy/setup-wifi.json` (0600). Keep the card with the robot as a fallback for the eye QR codes. Run it as `robot-buddy` so the service can read the file. Deleting the file makes a new password at the next setup; reprint the card then.

The full [Pip deployment](DEPLOYMENT.md) captures the microphone in the robot service. An external speech front end can still send wake and utterance lines to `/run/robot-buddy/control.sock` (mode 0660, group `pip-control`). Put its account in `pip-control` only, never in the `robot-buddy` group, which can read `/etc/robot-buddy`. To check, with `stt` as the speech account:

```sh
id stt                                                # pip-control, not robot-buddy
sudo -u stt cat /etc/robot-buddy/device.identity.json # Permission denied (EACCES)
printf '{"wake":true}\n' | sudo -u stt socat - UNIX-CONNECT:/run/robot-buddy/control.sock
```

Upgrading an older install: run the `groupadd`, `usermod -aG pip-control robot-buddy`, `gpasswd -d stt robot-buddy`, then `chown robot-buddy:robot-buddy` and `chmod 0600` the identity file, install the new unit and `40-robot-buddy-nm.rules` (removing any legacy `/etc/polkit-1/rules.d/50-robot-buddy-nm.rules` after checking site polkit policy), and restart.

When the robot cannot join a network, it opens its setup hotspot. Join it using the printed card or left-eye QR code, then scan the right eye or visit `http://10.42.0.1` and enter home Wi-Fi credentials. The optional kiosk panel (`BUDDY_SCREEN_ADDR`, default `127.0.0.1:8090`) also shows setup information. A mistyped password leaves any working home network in place. Once connected, sign in with the configured OIDC provider from the claim link and confirm the matching six-digit code by speaking it to Pip or using the kiosk panel. No phone app is needed.

For development, `cargo run -p robot-buddy-robot -- --simulate` skips NetworkManager and the built-in mic, neck and camera. It accepts text or JSON control lines on stdin (`{"say":…}`, `{"wake":true}`, `{"listening":false}`, `{"mute":true}`) and emits ordered events on stdout: full JSON in simulation, compact lines without conversation text on hardware. Set `BUDDY_SPEECH=true` for ALSA playback and `BUDDY_FACE_SERIAL` for the ESP32 face. The [robot client guide](docs/architecture/robot-client.md) has the input contract and face states. [DEPLOYMENT.md](DEPLOYMENT.md) covers the Pi build, assets, calibration, ownership claim, and hardware smoke tests. Physical acceptance is still outstanding. HTTP server URLs are allowed only for explicit localhost simulation.

## Architecture and validation

- [Implemented architecture and diagrams](docs/architecture/backend.md)
- [Rig API research and source references](docs/research/rig-integration.md)
- [Implementation status and remaining acceptance](docs/plans/implementation.md)
- [First-boot design](docs/architecture/boot-and-registration.md)
- [Rivet documentation research](docs/research/rivet-actor-findings.md)

```sh
bun run format:check
bun run check
bun run lint
bun run test
bun run test:integration
```

The opt-in integration suite starts an isolated engine on port 16420 and a model fixture on 19091. It exercises the actual Rig/Rivet stack without cloud credentials. `apps/server/tests/container.rs` provides an opt-in first-boot smoke test for a separately running isolated container. Live OpenRouter, OIDC provider, Wi-Fi hardware and speech acceptance require your configuration/devices.

## Browser workbench

Enable the optional `/lab` UI to test device enrollment, owner QR registration, chat, history and memory. See [setup and browser tests](docs/lab.md).
