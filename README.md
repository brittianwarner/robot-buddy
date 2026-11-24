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
install -m 0644 infra/robot-buddy-nm.rules /etc/polkit-1/rules.d/50-robot-buddy-nm.rules
install -m 0644 infra/robot-buddy.service /etc/systemd/system/
install -d -m 0700 -o robot-buddy -g robot-buddy /var/lib/robot-buddy
runuser -u robot-buddy -- /usr/local/bin/robot-buddy-robot setup-card
systemctl daemon-reload && systemctl enable --now robot-buddy
```

The unit starts beside NetworkManager and does not wait for the network to come up. Do not enable `NetworkManager-wait-online.service` for it.

`setup-card` prints this robot's setup Wi-Fi name, password and a QR code, created once and kept in `/var/lib/robot-buddy/setup-wifi.json` (0600). Print it and keep it with the robot: it is the only way to reach setup on a robot without a screen. Run it as `robot-buddy` so the service can read the file. Deleting the file makes a new password at the next setup; reprint the card then.

The speech process sends wake and utterance lines to `/run/robot-buddy/control.sock` (mode 0660, group `pip-control`). Put its account in `pip-control` only, never in the `robot-buddy` group, which can read `/etc/robot-buddy`. To check, with `stt` as the speech account:

```sh
id stt                                                # pip-control, not robot-buddy
sudo -u stt cat /etc/robot-buddy/device.identity.json # Permission denied (EACCES)
printf '{"wake":true}\n' | sudo -u stt socat - UNIX-CONNECT:/run/robot-buddy/control.sock
```

Upgrading an older install: run the `groupadd`, `usermod -aG pip-control robot-buddy`, `gpasswd -d stt robot-buddy`, then `chown robot-buddy:robot-buddy` and `chmod 0600` the identity file, install the new unit and rules, and restart.

On first boot the robot opens its setup hotspot. Join it with the password or QR from the card, then visit `http://10.42.0.1` in the phone browser and enter home Wi-Fi credentials. A kiosk browser on the face UI (loopback port 8090) shows the same QR. A mistyped password leaves any working home network in place. After connection, the face shows a registration QR; sign in using the configured OIDC provider and confirm the matching code on the robot. No phone app is needed. Automatic captive-portal opening is not implemented; the browser URL is the fallback.

For development, `cargo run -p robot-buddy-robot -- --simulate` skips NetworkManager. It accepts text or JSON control lines on stdin (`{"say":…}`, `{"wake":true}`, `{"listening":false}`, `{"mute":true}`) and emits ordered events on stdout: full JSON in simulation, compact lines without conversation text on hardware. Set `BUDDY_SPEECH=true` for ALSA playback and `BUDDY_FACE_SERIAL=/dev/ttyACM0` for the ESP32 face. The [robot client guide](docs/architecture/robot-client.md) has the input contract, face states, ALSA settings and the Raspberry Pi 5 build. Actuator and camera drivers are not included, and none of this has been tested on robot hardware. HTTP server URLs are allowed only for explicit localhost simulation.

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
