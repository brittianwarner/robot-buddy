# Robot Buddy

Independent Rust project. Backend and device application code stays Rust. Bun/Turborepo runs Cargo tasks; use `bun install`, `bun run check`, `bun run lint`, `bun run test`, `bun run format:check`, and `bun run test:integration`. `bun run check:all` runs the whole local gate in CI order and stops at the first failure ([testing](docs/testing.md)). Wall-clock limits in tests fail only under `BUDDY_STRICT_TIMING=1`; otherwise assert ordering and functional facts.

Read [architecture](docs/architecture/backend.md), [acceptance status](docs/plans/implementation.md), and [Rig source findings](docs/research/rig-integration.md) before changing the agent runtime. Pin Rig checkpoint format with its crate version and handle migrations explicitly.

Bot owns identity/ownership, Conversation owns transcript and Rig execution, Memory owns facts. Actor struct fields are transient; durable application state goes in actor SQLite. Do not add a global dispatcher actor that owns all devices. Concurrent actions require transactions, unique keys or conditional writes. Wire changes are additive and readers tolerate unknown fields (`crates/buddy/src/protocol.rs`). Actor errors reach clients masked as internal; the `user` group is public, with message and metadata, so use it only for an outcome a device acts on (today `user/busy`, naming the open turn).

Use Rig's provider adapters and AgentRun instead of another SSE decoder or agent loop. Persist effect intent before execution and result before advancing. Do not repeat an uncertain side effect automatically. Keep inference context, checkpoint size, tool count and streaming buffers bounded. Canonical history remains outside the prompt. Measure release builds before claiming performance gains.

The robot uses the official Rivet Rust client. Application sessions and engine gateway grants both restrict access. Never log session tokens, device credentials, QR claim tokens, prompts or model-provider keys. Never commit real env files or identities. Fixtures use isolated ports and test-only credentials.

The Docker image contains the server and engine; its mounted volume holds log and filesystem object storage. One engine owns that directory. Local volume support does not imply high availability or replace Rivet's production object-storage recommendation.

No live hardware, cloud-provider or browser acceptance is implied by compilation or fixture tests. Record the actual validation performed. No remote repository or deployment is assumed.

Pip has one device service and one Rivet session. Keep microphone, face, neck and camera ownership in `apps/robot`; transient `face.express` pushes are actionable only for a contiguous live event on the locally admitted turn, never from replay. Never arm a neck without a complete per-device calibration and matching locked servo EEPROM limits. Face recognition stays local and never grants access. Piper/espeak-ng run out of process. See [deployment and bench checks](DEPLOYMENT.md) before changing hardware defaults or systemd limits.

The optional browser lab is operator-only and disabled without BUDDY_LAB_TOKEN. Its action allowlist uses device-scoped sessions; do not expose admin actor actions or private device keys to server routes. Browser acceptance: [lab guide](docs/lab.md), `bun run test:lab`.

Speech accepts a turn ID, never caller-supplied text. The PCM adapter is OpenRouter `elevenlabs/eleven-v4-turbo` only: premade voice name, plain answer text, `provider.zdr`, and no Fish options or speed field. Will is the default speaker. Ordinary chat streams the provider response-text channel in one inference call. Reasoning and unknown payloads never enter speech; tool-started streams stay silent. The prompt requires response text to contain only spoken dialogue. Tool work uses a separate tool-disabled final phase. Reasoning and action-phase prose are excluded from speech and from spoken-phase context. Voice settings are pinned at speech admission; interrupted streams are not replayed automatically. Audio is transient bounded HTTP PCM, not actor state. Keep the shared [voice contract](docs/architecture/voice.md) aligned across browser and Linux clients.

Conversation owns revisioned persona, curated voice and location/timezone settings. Runtime date/time is derived before every model request, never embedded permanently in the editable persona. Named persona versions pair prompt and voice; activation updates both atomically. Settings tools use compare-and-swap revisions and atomic receipts; location labels are data, not instructions.
