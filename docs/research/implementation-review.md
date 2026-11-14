# Implementation review

Reviewed the Rust server, device bootstrap, Bot/Conversation/Memory actors, Rig provider/AgentRun integration, and container boundary. Local findings and verification below apply to this review; they are not a production security certification.

## Fixed findings

| Severity | Finding | Resolution |
| --- | --- | --- |
| High | OIDC callback trusted userinfo without validating its ID token. | Validate JWKS signature, allowed algorithm, issuer, audience/authorized party, nonce, expiry, issue time and matching subject. Bound all identity-provider response bodies. |
| High | Browser form security policy prevented external OIDC redirects. | Permit only the configured authorization origin in the registration form policy. Verified in Chromium through callback and device confirmation. |
| High | Provider tool IDs reused in later rounds could replay the wrong durable receipt. | Namespace IDs by completion round, and require receipt name/arguments to match. Real-engine fixture intentionally repeats one ID with two different values. |
| Medium | Socket delivery/auth errors could fail otherwise durable generation or actor recovery. | Commit output first; socket delivery is best effort. Replay remains authoritative. |
| Medium | Cancelled turns could receive late events or recreate Rig checkpoints. | Condition event/intent/checkpoint writes on running status. Verify cancellation retains partial output and emits no subsequent output. |
| Medium | Restart/cancellation discarded partial output from canonical history. | Transactionally assemble persisted deltas when finalizing noncompleted turns. |
| Medium | Model fanout across bots had no process-wide bound. | Shared semaphore, configurable 1–256 simultaneous model requests, default 16, fail fast at capacity. Shared provider connection pool, explicit connect/total deadlines, no redirects. HTTP requests also fail fast above 128 concurrent handlers and expire after 30 seconds. |
| Medium | Raw provider payloads duplicated checkpoint data; producer retained a sender outside its task. | Persist only normalized Rig turn state; move stream sender into its producer so closure cannot leave the consumer waiting indefinitely. |
| Medium | Snapshot/replay handoff could duplicate or omit text in a browser reconnect. | Read transcript and replay cursor in one SQLite transaction; browser resumes after that cursor. Submission retries reuse a request ID and refresh durable status after acknowledgement. |

## Actor and harness boundaries

Bot owns enrollment, challenge replay protection, ownership and revocation. Conversation owns ordered turns, full durable transcript, output events, Rig checkpoints, and tool receipts. Memory owns facts and its search index. There is no fleet-wide dispatcher or model-history blob on Bot. Per-bot generation is serialized; other bots can run concurrently within the global limit.

Rig remains the provider adapter and run state machine. Rivet owns persistence and actor lifetime. A checkpoint before a model request records an uncertain boundary: restart interrupts that request instead of blindly billing/reissuing it. Ready checkpoints can resume; unknown tool outcomes fail closed. This does not claim distributed exactly-once effects across actors or external services. The current memory operations are assignments/deletions, not arbitrary irreversible robot actions.

Full history is retained, with indexed search and bounded pages. Prompts contain recent completed turns and bounded selected facts; they do not resend the entire transcript. Tool loops, output, checkpoints, stream buffers, provider duration and per-turn duration have limits. No release throughput or latency improvement is claimed without load measurements.

## Browser lab

The optional `/lab` route is disabled by default. It requires an operator secret and a signed provisioned identity to bind a simulated device. Private device keys stay in browser memory. Cookie, Origin, CSRF, command allowlist, CSP, safe text rendering and no-store responses protect the operator workflow. The browser has no engine admin credential. See [lab instructions](../lab.md).

## Validation and limits

Passed: six Rust unit tests (including an OIDC valid token and eight invalid binding/signature cases), strict workspace Clippy, formatting, real-engine actor integration with reused tool IDs/cancellation/restart, and isolated headless Chromium enrollment→OIDC→confirmation→Rig memory tool→chat→reload. Desktop and mobile screenshots were inspected. The model and OIDC provider in browser acceptance were controlled fixtures, not real cloud services.

Still requires deployment acceptance: selected local-model tool behavior, real OIDC-provider policies, Linux NetworkManager and physical controls, release-load measurements, ingress rate limits, backup/restore, and storage retention policy. A model's instruction to remember only explicitly requested facts is not a separate human approval mechanism. No filesystem, arbitrary code runner or physical movement tool is exposed. User deletion and a separately concurrent new memory write follow actor arrival order; there is no global causal tombstone protocol.

JWT validation uses the already-present AWS-LC backend instead of introducing `rsa` 0.9 through the RustCrypto backend. This avoids that dependency's [published private-key timing advisory](https://rustsec.org/advisories/RUSTSEC-2023-0071.html); application OIDC verification itself uses public keys. A full cargo-audit scan was not performed (the tool was not installed).

Subsequent voice work added a completed-answer-only speech action, authenticated bounded PCM forwarding and matching browser/Linux playback contracts. Six unit tests and real-engine tests passed, including suppression of a synthetic reasoning marker, refusal to synthesize running turns, and device-role enforcement. Live GLM Nitro and Drama 3 browser playback/stop/reload acceptance passed. See [voice architecture](../architecture/voice.md).

The configuration extension passed 12 Rust unit tests, strict Clippy and real-engine restart/atomic-activation integration. Time tests cover daylight-saving transition and local-date rollover. Named profile activation was tested with a stale voice revision to verify prompt rollback; saved prompt, voice, timezone and profile versions survive restart. Speech-rule tests cover word boundaries, punctuation, phoneme validation and transcript-preserving rendering.

Live isolated GLM acceptance created and activated Action Buddy from an “act like” request, restored Buddy version 1, changed the voice/timezone, and verified settings through actor readback. Browser PCM playback/Stop/replay, silent reload and mobile overflow checks passed. A separate provisioned test identity avoids changing the user's active robot. The initial activation failure exposed a model-supplied concurrency-token problem: activation tools now obtain revisions in application code; direct client commands keep explicit compare-and-swap inputs.

OpenRouter accepted explicit curated voice IDs and Fish provider options. Controlled speed probes showed little change when speed was supplied only under provider options, but substantially different output durations using OpenRouter's top-level speed field; the adapter uses that field. Other numeric controls are forwarded according to the documented provider-options contract; independent acoustic verification of every control and subjective voice quality remains outstanding. The optional web-search tool is omitted when its provider key is not configured, avoiding an advertised but unavailable tool.

The final isolated browser test added an emphasis rule through the live agent, verified it in actor state and the settings view, then exercised speech playback/Stop/replay and silent reload. An initial false verbal save confirmation prompted explicit current-turn tool-receipt context and stronger write-before-confirm instructions; the rerun persisted the rule and passed. This is observed acceptance, not a guarantee that model-generated claims can never be wrong. The application still enforces actual state writes and speech authorization in code.

## Sentence-stream speech acceptance

The OpenRouter-only path now separates silent Rig tool work from tool-disabled spoken generation. Fifteen Rust unit tests, strict workspace Clippy, formatting, and real-engine integration passed. Integration verifies that synthetic reasoning and planner prose cannot enter spoken context/output, that a sentence is available before completion, that voice settings are pinned, and that cancellation/restart stop further delivery. The Rust pipe-player test consumes audio while its HTTP response remains open.

An isolated Chromium fixture verified incremental WebAudio playback and abort. Live OpenRouter acceptance with a separate test identity observed first audio before completion on a long reply, stopped the turn, reloaded silently, and played a subsequent three-sentence reply to completion through one client stream per turn. The rendered desktop lab was inspected. This does not validate physical ALSA playback, subjective prosody continuity, or load performance.

## Direct conversation latency correction

Ordinary conversation now streams the provider response-text channel directly in its first inference call. The extra mandatory planning call and experimental marker gate are removed. Reasoning/unknown events remain excluded by the Rig adapter; tool-started streams remain silent, and a late tool call after dialogue fails without executing. Response text is instructed to contain spoken dialogue only; the transport cannot semantically distinguish narration incorrectly placed in that channel.

Workspace tests, strict Clippy and real-engine integration passed. The integration counts exactly one provider call for a normal greeting and verifies synthetic reasoning exclusion, early speech chunks, pinned voice, tools, cancellation and restart. A live OpenRouter greeting logged the direct route: first text at 925 ms and first audio at 2067 ms (1130 ms TTS startup). The prior two-pass sample was 1612 ms to text and 2173 ms to audio; these are individual observations, not a statistically established speedup. Physical robot audio timing remains unverified.
