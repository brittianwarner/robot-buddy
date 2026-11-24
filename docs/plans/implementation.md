# Implementation status

Implemented core: Rust Cargo workspace managed through Bun/Turborepo; pinned native Rivet runtime/client; Rig provider streaming and serialized run checkpoints; OpenRouter/local configuration; separate Bot, Conversation and Memory actors; FTS5 history/facts; web-search and explicit memory tools; deduplicated submissions and durable output replay; app-free NetworkManager setup/face QR; certificate challenge enrollment and OIDC ownership confirmation; single-image engine/server packaging with mounted storage.

The real-engine integration suite covers registration idempotency, bot isolation, Rig text and memory-tool execution, history retrieval, terminal cancellation idempotency, nonce replay rejection, owner claim, restart persistence, interrupted streaming, and revocation. Container smoke tests cover bootstrap HTTP and scoped official client access. Test output is the evidence for any particular revision; compilation alone does not establish runtime behavior.

## Pip hardware port

The `feat/pip-hardware` branch adds the on-device client: local listening (VAD, whisper-cli, transcript and optional neural wake), the calibrated 50 Hz STS3215 neck, the ESP32-S3 face protocol, camera tracking and an optional local face gallery, the offline Piper line, and the loopback operator panel. See [CHANGELOG.md](../../CHANGELOG.md).

Evidence recorded so far. None of it is from a physical Pip:

- ARM64 Bookworm and Trixie toolchain matrix green at `82e8d5b` ([evidence](../../infra/toolchain-validation.md)).
- 30-minute virtual-Pip E2E soak passed at `82e8d5b`, with real ARM64 binaries, Rivet 3.1.1, whisper-cli and virtual peripherals. Summary in [CHANGELOG.md](../../CHANGELOG.md#validation-status). Soak report (artifact `soak-30m-c5d7c08`) kept outside the repo; available from the release coordinator.
- Fresh ARM64 systemd deployment rehearsal passed on `5351762`, including OIDC claim against a fixture IdP, upgrade and rollback ([evidence](../../tests/pip-deploy-qa/EVIDENCE.md)).
- Asset-dependent wake and vision checks on `5351762` ([release gates](../testing-pip-release.md)).
- Workstation e2e latency republished at `82e8d5b` ([table](../research/e2e-latency.md)).

## Remaining acceptance

These need real inputs, accounts or hardware. No live acceptance is recorded for them:

- The chosen local inference server and its tool-calling support. OpenRouter GLM Nitro/low was exercised live, together with the earlier Fish Drama 3 speech route. See the [validation record](../research/validation.md) for scope.
- Live OpenRouter `elevenlabs/eleven-v4-turbo` speech. The current speech route has fixture and mock-provider tests only (including the E2E soak's mock PCM provider).
- A real OIDC application configured with the callback URL, and a phone owner-claim walkthrough. Browser ownership and the rehearsal's claim used controlled OIDC fixtures.
- A physical NetworkManager AP-mode adapter, the kiosk display, wrong-password retry and power-loss testing.
- Physical Pip: natural "Hey Pip" in a room, mic SNR and echo, servo calibration, direction, stops and torque release, OV5647 exposure and lens, Pi 5 thermal and voltage under the real cgroup limits, and acoustic latency. The procedure is the [DEPLOYMENT.md checklist](../../DEPLOYMENT.md#5-hardware-bring-up-checklist) with [performance-pip.md](../performance-pip.md).
- Server operations: storage sizing, a backup/restore drill, ingress limits, and the rivet-engine memory growth seen in the soak.

Explicitly deferred product extensions: reminders/scheduling, arbitrary page fetch/browser operation, attachments, filesystem/code execution, semantic/vector retrieval, conversation summaries, multiple conversations per bot, owner transfers/factory reset and arbitrary third-party integrations. These are not represented as implemented capabilities. The earlier first-boot and client-contract documents describe broader intended design; the implemented architecture is authoritative for this version.

The optional operator browser lab covers the post-Wi-Fi enrollment, QR claim, chat, history, memory and activity flow. See [lab guide](../lab.md) and [implementation review](../research/implementation-review.md).
