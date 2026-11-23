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

Copy the three generated values from `secrets/secrets.env` into `.env`. Set `AI_MODEL`, `AI_API_KEY`, public HTTPS URLs, and OIDC sign-in settings. For a local model, set **both** `AI_PROVIDER=openai_compatible` and `AI_BASE_URL` (for example `http://host.docker.internal:11434/v1` on Docker Desktop). Do not commit secrets. The offline `issuer.key` provisions device credentials and is not mounted into the server.

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

Install the robot binary on Linux with NetworkManager and a Wi-Fi adapter supporting AP mode. Set `BUDDY_SERVER_URL`, `BUDDY_IDENTITY_FILE`, `BUDDY_CACHE_FILE`, and optionally `BUDDY_WIFI_INTERFACE` (default `wlan0`). The example [systemd unit](infra/robot-buddy.service) runs with the privileges needed to manage NetworkManager profiles. Provision each robot a unique identity file, readable only by its service user.

The robot exposes its face UI on loopback port 8090; display it in a local kiosk browser. First boot shows a Wi-Fi QR for a temporary protected hotspot. Connect, then visit `http://10.42.0.1` in the phone browser and enter home Wi-Fi credentials. After connection, the face shows a registration QR; sign in using the configured OIDC provider and confirm the matching code on the robot. No phone app is needed. Automatic captive-portal opening is not implemented; the browser URL is the fallback.

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
