# Local validation

Validated on November 13, 2026 using the code in this repository. This is local acceptance, not a deployed production service.

- Formatting and workspace Clippy (`--all-targets --all-features --locked -D warnings`) passed.
- Five unit tests passed, including OIDC token validation; the other cases cover: signature purpose/key binding, device certificate/proof binding, NetworkManager keyfile injection rejection, and escaping.
- The real Rivet integration scenario passed against engine 3.1.1 with Rig 0.46.0 and a local OpenAI-compatible streaming fixture. Assertions cover text, memory tool execution, history lookup, request deduplication, isolation, consumed enrollment nonces, claim confirmation, cancellation idempotency, revoked access, completed state across engine restart, and interruption of an active model stream.
- The single-container release image built successfully on Linux ARM64 as `robot-buddy:local`.
- The HTTP bootstrap/container scenario passed: certificate enrollment, rejection of replayed proof, stable actor IDs across re-enrollment, official Rust client access with the issued gateway grants, and reading previously stored memory after replacing the container while retaining its volume.

The unit-test command intentionally skips external integration cases; these were run separately. Initial development failures included an omitted test namespace, an incomplete SSE fixture, and a collision between concurrent test runs on the same fixture port. The final isolated runs passed after those issues were corrected.

Not validated: a local inference server's tool support, a real-provider OIDC browser login, NetworkManager/AP hardware, kiosk/speech hardware, Linux AMD64 image execution, production load or a backup restoration drill. Those require the operator's selected services and robot. No remote repository, public deployment or provider credentials were created.

The review added automated Chromium acceptance with a controlled OIDC provider and streaming model fixture. Registration, reconnect, owner confirmation, Rig memory tools/chat, durable reload, rejected unauthorized/CSRF/admin commands, and mobile layout passed. See [review findings](implementation-review.md) and [lab instructions](../lab.md).

Live OpenRouter acceptance also passed with GLM 5.3 Flash Nitro (low reasoning) and Fish Drama 3: memory-tool execution, conversational answer, streaming PCM playback in Chromium, Stop, explicit replay and silent reload. The operator supplied the key; it is stored only in ignored local configuration. This validates browser playback mechanics, not subjective voice quality or physical speaker output.
