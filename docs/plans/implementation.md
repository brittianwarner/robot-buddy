# Implementation status

Implemented core: Rust Cargo workspace managed through Bun/Turborepo; pinned native Rivet runtime/client; Rig provider streaming and serialized run checkpoints; OpenRouter/local configuration; separate Bot, Conversation and Memory actors; FTS5 history/facts; web-search and explicit memory tools; deduplicated submissions and durable output replay; app-free NetworkManager setup/face QR; certificate challenge enrollment and OIDC ownership confirmation; single-image engine/server packaging with mounted storage.

The real-engine integration suite covers registration idempotency, bot isolation, Rig text and memory-tool execution, history retrieval, terminal cancellation idempotency, nonce replay rejection, owner claim, restart persistence, interrupted streaming, and revocation. Container smoke tests cover bootstrap HTTP and scoped official client access. Test output is the evidence for any particular revision; compilation alone does not establish runtime behavior.

Remaining acceptance requires actual inputs and hardware:

- The chosen local inference server and its tool-calling support. OpenRouter GLM Nitro/low and Drama 3 have been exercised locally; see the review record for scope.
- A real OIDC application configured with the callback URL, plus a phone owner-claim walkthrough. Automated browser ownership was validated against a controlled OIDC fixture.
- Linux NetworkManager AP-mode adapter, kiosk display, wrong-password retry and power-loss testing.
- Hardware speech input/output integration and actuator-specific safety/acknowledgement semantics.
- Release-build load measurements; storage sizing, backup/restore drill and operational ingress limits.

Explicitly deferred product extensions: reminders/scheduling, arbitrary page fetch/browser operation, attachments, filesystem/code execution, semantic/vector retrieval, conversation summaries, multiple conversations per bot, owner transfers/factory reset and arbitrary third-party integrations. These are not represented as implemented capabilities. The earlier first-boot and client-contract documents describe broader intended design; the implemented architecture is authoritative for this version.

The optional operator browser lab covers the post-Wi-Fi enrollment, QR claim, chat, history, memory and activity flow. See [lab guide](../lab.md) and [implementation review](../research/implementation-review.md).
