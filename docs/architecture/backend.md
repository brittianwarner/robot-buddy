# Implemented backend architecture

The backend application and robot client are Rust. Versions are pinned in Cargo.lock and Dockerfile. Rig drives model/tool sequencing; Rivet owns persistence and actor lifecycle. The robot owns microphone/speaker/camera/actuator integration. AgentOS and a general-purpose agent filesystem are not needed for the implemented tools.

```mermaid
flowchart LR
  Phone[Phone browser: Wi-Fi and owner claim] --> Boot[Rust bootstrap HTTP service]
  Robot[Linux robot / official Rivet Rust client] --> Engine[Rivet engine]
  Boot --> Engine
  subgraph Container[Single Docker container]
    Engine <--> Bot[Bot actor: identity and ownership]
    Engine <--> Chat[Conversation actor: transcript and Rig run]
    Engine <--> Memory[Memory actor: indexed facts]
    Chat --> Rig[Rig AgentRun + provider transport]
    Chat --> Memory
    Rig --> Tools[Bounded tool executor]
    Tools --> Memory
    Boot
  end
  Rig --> Router[OpenRouter default]
  Rig --> Local[Configured local OpenAI-compatible server]
  Tools --> Search[Brave search]
  Engine --> Volume[Mounted volume: logs + filesystem objects]
```

## Ownership

| Actor | Key | Durable state |
|---|---|---|
| Bot | Stable hash of provisioned device ID | Device key binding, revocation, owner, pending QR claim, consumed enrollment nonces |
| Conversation | Bot ID (one conversation per robot initially) | Turns, deduplication IDs, tool intents/results, ordered events, versioned Rig checkpoint, bounded persona library and voice/location settings |
| Memory | Bot ID | Explicit facts and SQLite FTS5 search index |

Bot does not run inference or hold transcripts. Memory has an independent mutation boundary. Conversation keeps its turn and transcript together, avoiding distributed commit for each token. Actor fields hold reusable clients and transient synchronization only. SQL transactions and unique constraints govern concurrent admission; an actor does not imply serialized asynchronous actions.

## Rig execution and recovery

```mermaid
stateDiagram-v2
  [*] --> Queued: persist unique client request
  Queued --> Running: SQL admission
  Running --> model_pending: checkpoint before each model request
  model_pending --> Completed: direct dialogue + sentence streaming
  model_pending --> ready: tool calls or planning response
  ready --> ready: tool intent, effect, result, checkpoint
  ready --> model_pending: next action-phase model request
  ready --> speech_pending: Rig run done, pin persona + voice
  speech_pending --> Completed: stream sentences, then final answer + event
  model_pending --> Interrupted: actor process stops
  speech_pending --> Interrupted: actor process stops
  ready --> Queued: restart resumes the checkpoint
  Running --> Cancelled: atomic terminal event
  Running --> Failed: deadline, provider failure, or a resumed intent without a result
```

The checkpoint `phase` column holds `model_pending`, `ready` or `speech_pending`. There is no separate tool phase: a tool intent is a `tool_calls` row without a result, written while the run is `ready`.

`AgentRun` is checkpointed at effect boundaries, not on every token. Snapshots carry a Rig-format version and are limited to 512 KiB. Completed/failed run checkpoints are deleted after their terminal event commits; canonical history and tool receipts remain. Unsupported checkpoint versions fail explicitly.

A restarted pending model request is interrupted, preserving committed output events; it is not automatically repeated. A ready checkpoint resumes. The `speech_pending` checkpoint interrupts after restart; spoken output is never regenerated automatically. A completed tool receipt is reused. An intent without a result fails closed because the effect may have happened. This is not an exactly-once claim for external services. Model calls have a 90-second deadline, a run has a 180-second execution deadline, and Rig permits eight action-phase model rounds with at most eight calls per round, followed by one tool-disabled spoken model call when tools require it. Ordinary dialogue completes in the first call. Recovery at a ready boundary resets the execution deadline; model-turn count remains in Rig state.

Terminal status and terminal event commit in one transaction. Failed, interrupted and cancelled turns retain committed partial text in history. Event and checkpoint writes require running status, and snapshot reads include an atomic replay cursor. Cancellation is idempotent and cannot relabel a completed turn. WebSocket events are hints; clients replay the durable sequence log. The robot persists an unacknowledged submission ID before sending and resends the same request after reconnect. Output consumption is at-least-once around a device crash: hardware speech playback needs its own acknowledgement policy.

## Context and performance

Normal prompts load the latest 12 completed turns (bounded text, read through the `turns_completed` index), up to six matching facts plus two recent facts, and the current request. Full transcript data stays in SQLite and is accessible via history pages and an FTS5 history-search tool. This provides bounded normal context cost; lexical retrieval is not semantic search or a promise that every relevant fact is automatically retrieved. Tool receipts retain tool arguments/results separately from the final spoken answer.

Rig uses a shared pooled HTTP transport with redirects disabled and connect/total deadlines; the web-search client is reused. Streaming uses a bounded channel and batches persisted deltas at 100 ms between sentence boundaries. Actor lookup is not performed per token. Device action authorization is checked fresh; active generation and event delivery share a one-second revocation-check cache, bounding revocation propagation while reducing cross-actor reads. Output events go only to connections subscribed to `output`. Each connection's session is verified once and its expiry cached. A connection whose session has expired is disconnected before the event is sent. A failed revocation check disconnects every connection. A process-wide inference semaphore defaults to 16 concurrent model calls and rejects overflow; no global actor is involved. No actor is kept awake after its registered work finishes. Robot status polling currently wakes the Bot actor every three seconds while connected.

Do not claim throughput improvements without measurement. Establish release-build baselines for first-token latency, SQLite operations per turn, checkpoint bytes, event bytes, memory per active run, and idle wakeups before changing batching or context limits. Semantic memory, summarization, multi-conversation indexing and event-retention policies are future extensions.

## Enrollment and storage

A provisioned Ed25519 key proves possession of an issuer-signed device identity. The server signs a short-lived challenge; its nonce is consumed in the Bot actor. A deterministic bot ID recovers the same actors even when the robot cache is lost. Scoped engine tokens grant access to exactly the bot, conversation and memory actor IDs; application sessions additionally bind bot, role and expiry. OIDC authorization-code/PKCE, a signed ID token validated against issuer JWKS, nonce/audience/expiry checks and matching userinfo bind an owner, followed by physical confirmation. There is no shared factory credential.

The container runs server and engine under a non-root user with graceful shutdown supervision. Both `storage.log_directory` and `object_storage.primary.file_system.path` are mounted under `/data`; no S3 is required for this single-process topology. Never use an in-memory object backend for restart persistence. Never run two engines on the same directory or place log storage on NFS/EFS.

Rivet documents filesystem objects for single-machine/quickstart use and recommends external object storage for production. Optional S3 settings are available without changing actor code. A single container remains one failure domain even with S3; high availability requires a different deployment. Sources: [storage](https://rivet.dev/docs/deploy/self-host/control-plane/storage/), [Rig release](https://github.com/0xPlaygrounds/rig/releases/tag/v0.46.0).

## Early speech boundary

The initial request can stream response-text dialogue directly, completing in one call. Tool/planning prose produces no user-visible text. After tools finish, Conversation pins persona and voice in a transaction and starts a tool-disabled response phase. Its deltas commit transcript text before sentence chunks. Clients react to `speech.ready` (with a snapshot fallback), then consume one authenticated PCM stream from the server. Sentence text/configuration are durable actor data; audio remains transient. See [voice delivery](voice.md).

## Streaming and cancellation

The provider runs on its own task. It feeds a bounded channel of 16 deltas. Storage writes never block it from reading the network.

At each sentence boundary, one transaction commits the pending `text.delta`, the speech chunk and its `speech.ready`. Every insert is guarded on the running turn. Events are delivered after the commit. Replay order is unchanged: text first, then each chunk and its ready event.

Each running turn has an in-process stop signal. A terminal cancel or failure fires it after its transaction commits. The stream consumer then returns at once and the provider task is aborted, which closes the provider connection. The consumer handles each wake-up after `select!` returns, so no SQLite transaction is dropped mid-flight. Durable turn status stays the source of truth. Checkpoints, tool intents and chunk writes use `RETURNING` and fail with "turn cancelled" on zero rows.

Before the first model call, memory search, persona and recent history load concurrently. The `model_pending` checkpoint commits before the request is sent. The spoken phase pins the voice while context and facts load. The pinned voice holds the chosen voice only, not the voice catalog.

One model call may stream 128 KiB of text and 256 KiB in total. The total counts reasoning, tool arguments and unknown provider payloads. Reasoning is counted and dropped, never forwarded.

## Sentence splitting

The splitter keeps a cursor and scans each byte once. A boundary is decided only when the next character arrives, so token fragmentation never changes the output. Chunks concatenate back to the exact model text. Whitespace after a boundary stays with the chunk it ends; whitespace-only chunks are not spoken.

- `!` and `?` end a sentence before whitespace. CJK terminators always do.
- Runs like `?!` and closing quotes or brackets stay with their sentence.
- `.` and ellipses end a sentence before whitespace, unless the next word is lower case.
- Listed abbreviations, initials, dotted abbreviations, list numbers and `3.14` or `example.com` do not split.
- A newline ends a chunk.
- The first chunk may end early at `,` `;` `:` or `—` plus whitespace once it holds 24 bytes.
- Unpunctuated text breaks at whitespace after 240 bytes and anywhere after 600.

If the stream stalls for 200 ms on an undecided sentence end, the harness decides it as a boundary. Only that case depends on timing.

## History search storage

`history_rows` maps each turn to a stable FTS row ID. The answer trigger replaces a turn's search row by that key. Version 1 deleted by the unindexed `turn_id` column, a full scan of the index for every finished turn. A one-time rebuild, recorded in `schema_migrations`, derives the index from `turns` again. The schema script is idempotent and no turn data is changed.

