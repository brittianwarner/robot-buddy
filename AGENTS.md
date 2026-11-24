# Robot Buddy

Independent Rust project. Backend and device application code stays Rust. Python appears only in optional test tooling and fixtures (`tests/pip-e2e/**`, `scripts/pip-perf.py`), never in the shipped robot or server. Bun (1.4.4) and Turborepo run the Cargo tasks. The Rust toolchain is pinned in `rust-toolchain.toml` (1.100.0).

Before you change the agent runtime, read [architecture](docs/architecture/backend.md), [acceptance status](docs/plans/implementation.md) and [Rig source findings](docs/research/rig-integration.md). [docs/README.md](docs/README.md) indexes everything else.

## Commands

Run from the repository root after `bun install` (`--frozen-lockfile` in CI). [docs/testing.md](docs/testing.md) says what each suite proves.

```sh
bun run format:check && bun run check && bun run lint && bun run test   # fmt, type check, Clippy -D warnings, unit + offline tests
bun run test:robot-features                                             # shared crate in the robot's no-default-features profile
cargo test --locked --all-features -p robot-buddy-robot                 # full Pip features; needs OpenCV dev, libclang, pkg-config
bun run test:integration                                                # real Rivet engine on 16420, model mock on 19091
bun run check:all                                                       # the whole local gate in CI order; stops at the first failure
bun run check:all -- --from integration                                 # resume at a named step
```

Wall-clock limits in tests fail only under `BUDDY_STRICT_TIMING=1`. Otherwise assert ordering and functional facts. Port variables (`BUDDY_TEST_ENGINE_PORT`, `BUDDY_TEST_MODEL_PORT`, `BENCH_PORT_BASE`) move every harness, so stacks can run side by side.

**ARM64 E2E harness** ([tests/pip-e2e](tests/pip-e2e/README.md)). It runs the real ARM64 robot, server and Rivet 3.1.1 with virtual peripherals in a `linux/arm64` container:

```sh
tests/pip-e2e/run-in-docker.sh build             # image robot-buddy-pip-e2e:arm64, or PIP_E2E_IMAGE
tests/pip-e2e/run-in-docker.sh prepare-cache     # checksum-verified public speech and vision assets
tests/pip-e2e/run-in-docker.sh run --suite smoke # then --suite all for the fault suite
```

**30-minute soak.** This is the exact form used for the `82e8d5b` qualifying run. Private paths are placeholders:

```sh
PIP_E2E_IMAGE=<e2e image tag> \
PIP_E2E_HOST_CACHE=<host cache dir> \
PIP_E2E_HOST_ARTIFACTS=<host artifacts dir> \
PIP_E2E_HEY_PIP_ONNX=<path to private hey_pip.onnx, mounted read-only> \
PIP_E2E_EXPECT_ROBOT_SHA256=<expected robot ELF sha256> \
bash tests/pip-e2e/run-in-docker.sh run --suite soak --duration 30m --out /opt/pip-e2e/artifacts/<name>
```

The defaults are `tests/pip-e2e/.cache` and `.artifacts`, both gitignored. `soak-gate.mjs` decides pass or fail. Its fixture tests run with `bun test tests/pip-e2e/soak-gate.test.mjs`.

**ARM64 toolchain matrix** ([evidence](infra/toolchain-validation.md)): `CARGO_JOBS=2 infra/run-arm64-matrix.sh both`. It refuses dirty workspace sources in gate mode. Run it on a clean tree with no soak competing for Docker.

**Deployment rehearsal** ([evidence](tests/pip-deploy-qa/EVIDENCE.md)): `bun tests/pip-deploy-qa/rehearsal.mjs`.

**Workstation latency bench** ([table](docs/research/e2e-latency.md)):

```sh
bun run bench:e2e --publish   # clean tree, quiet host; writes docs/research/e2e-latency.json
bun run bench:report          # regenerate the table in e2e-latency.md from the JSON
bun run bench:report:check    # CI: table matches JSON, JSON is clean and describes HEAD's measured paths
```

`--publish` refuses a dirty tree, and it refuses to write when the noise gate fails. The gate requires 1-minute load ≤ 0.5 per CPU at start and end, which is load ≤ 7 on the 14-CPU M4 Pro used so far. Close other builds first. Any change under `apps`, `crates`, `tests/lib`, `tests/bench-e2e.mjs`, `Cargo.toml`, `Cargo.lock` or `rust-toolchain.toml` makes the published JSON stale until it is republished. Never hand-edit `docs/research/e2e-latency.{json,md}`.

## Actors and the agent runtime

Bot owns identity and ownership, Conversation owns the transcript and Rig execution, and Memory owns facts. Actor struct fields are transient. Durable application state goes in actor SQLite. Do not add a global dispatcher actor that owns all devices. Concurrent actions need transactions, unique keys or conditional writes. Wire changes are additive, and readers tolerate unknown fields (`crates/buddy/src/protocol.rs`). Actor errors reach clients masked as internal. The `user` error group is public, with message and metadata, so use it only for an outcome a device acts on (today only `user/busy`, which names the open turn).

Use Rig's provider adapters and AgentRun. Do not write another SSE decoder or agent loop. Pin the Rig checkpoint format to its crate version and handle migrations explicitly. Persist effect intent before execution, and persist the result before advancing. Never repeat an uncertain side effect automatically. Keep inference context, checkpoint size, tool count and streaming buffers bounded. Canonical history stays outside the prompt. Measure release builds before claiming performance gains.

Inference providers are `AI_PROVIDER=openrouter` (default) or `openai_compatible`. Nothing else is accepted (`crates/buddy/src/model.rs`).

## Speech and persona

Speech accepts a turn ID, never text from the caller. The PCM adapter is OpenRouter `elevenlabs/eleven-v4-turbo` only: a premade voice name, plain answer text, `provider.zdr`, and no Fish options or speed field. Will is the default speaker. Ordinary chat streams the provider's response-text channel in one inference call. Reasoning and unknown payloads never enter speech, and tool-started streams stay silent. The prompt requires response text to contain only spoken dialogue. Tool work uses a separate tool-disabled final phase. Reasoning and action-phase prose stay out of speech and out of spoken-phase context. Voice settings are pinned when speech is admitted, and interrupted streams are not replayed automatically. Audio is transient, bounded HTTP PCM, never actor state. Keep the shared [voice contract](docs/architecture/voice.md) aligned across browser and Linux clients.

Conversation owns revisioned persona, curated voice and location/timezone settings. Runtime date and time are derived before every model request and never embedded permanently in the editable persona. Named persona versions pair prompt and voice, and activation updates both atomically. Settings tools use compare-and-swap revisions and atomic receipts. Location labels are data, not instructions.

## Device (Pip)

Pip has one device service and one Rivet session. The robot uses the official Rivet Rust client. Keep microphone, face, neck and camera ownership in `apps/robot`. A transient `face.express` push acts only on a contiguous live event for the locally admitted turn, never on replay. Never arm a neck without a complete per-device calibration and matching locked servo EEPROM limits. Face recognition stays local and never grants access. Piper and espeak-ng run out of process and are never linked. Read [DEPLOYMENT.md](DEPLOYMENT.md) before you change hardware defaults or systemd limits.

## Security and secrets

Application sessions and engine gateway grants both restrict access. Never log session tokens, device credentials, QR claim tokens, prompts or model-provider keys. Never commit real env files (`.env`, `.env.local`), `secrets/`, identities, private models (`hey_pip.onnx`), recordings or firmware sources. Docs use placeholders such as `<your-openrouter-key>`. Fixtures use isolated ports and test-only credentials.

The optional browser lab is operator-only and stays disabled without `BUDDY_LAB_TOKEN`. Its action allowlist uses device-scoped sessions. Do not expose admin actor actions or private device keys to server routes. Browser acceptance: [lab guide](docs/lab.md), `bun run test:lab`.

## Deployment and storage

The Docker image contains the server and the engine. Its mounted volume holds log and filesystem object storage, and only one engine may own that directory. Local volume support does not imply high availability or replace Rivet's recommendation of object storage for production.

## Evidence

Compilation and fixture tests do not imply live hardware, cloud-provider or browser acceptance. Record the validation you actually ran, the SHA it ran on, and what it does not prove. Keep evidence files ([toolchain-validation](infra/toolchain-validation.md), [EVIDENCE.md](tests/pip-deploy-qa/EVIDENCE.md)) as records: append or supersede, never rewrite history. No remote repository or deployment is assumed.
