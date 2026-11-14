---
title: Rivet actors and Rust examples
tags: [type/research, rivet, rust]
---

# Rivet actor and Rust source research

Research date: 2026-11-13. Proposal context: Rust server-side text agent, cloud APIs allowed; robot hardware outside server scope; official Rivet client integration is required.

## Coverage

- `actors-manifest.json`: fetched every one of the 54 `/actors/docs/` pages listed by official `https://rivet.dev/llms.txt`, including 428 fenced code blocks. Recursively followed links within that subtree until no new links remained. Four extra referenced paths returned 404 for their Markdown endpoint: access-control, general/edge, http-api, quickstart/next-js. 58 attempted URLs total, 54 successful.
- `rust-source-manifest.json`: 18 successfully downloaded release-pinned source files: all files in `examples/ai-agent-rust`, `examples/chat-room-rust`, `examples/hello-world-rust`, plus public actor/context/SQLite/lib API files, all at v3.1.1.
- `rivet-source-tree.json`: release-pinned full repository tree used to enumerate Rust examples.
- Deep review: Rust quickstart/client docs; actor lifecycle, actions, auth/permissions, connections/events, state/SQLite, schedules, limits, design patterns, production checklist, deprecated queues/run; all three Rust actor implementations and AI provider helper; Rust Actor/Ctx/transaction APIs. Remaining documentation is downloaded/indexed and its prose surveyed, not every code block manually audited. No examples compiled or executed. Do not represent download counts as execution or exhaustive manual review.
- Cross-subtree links (general docs, deployments, workflows, ecosystem, guides) are handed to the parent agent's crawl. External sites and all historical GitHub branches were not recursively traversed.

## Findings affecting design

1. Live GitHub release lookup returned stable v3.1.1, published 2026-11-11T23:05:26Z. Web search cache had stale v3.0.7. Pin SDK and engine together and record container digest in implementation. https://github.com/rivet-dev/rivet/releases/tag/v3.1.1
2. Rust actors are native and GA. Use public `rivetkit`, not bespoke runner/envoy bindings. https://rivet.dev/changelog/2026-11-06-rust-sdk-for-rivet-actors-is-now-generally-available/ ; https://rivet.dev/actors/docs/quickstart/rust/
3. Durable state belongs in per-actor SQLite. Actor fields hold transient clients/cancellation handles/cache only. No global god actor or actor for each stateless request. Conversation is the natural actor boundary; a run actor is justified only if it has independent durable lifetime. https://rivet.dev/actors/docs/state/ ; https://rivet.dev/actors/docs/design-patterns/
4. Actions run concurrently and interleave across awaits. Persist one active run per conversation with a conditional SQL transition; use request-id uniqueness and compare-and-set transitions, not presumed actor serialization. https://rivet.dev/actors/docs/actions/ ; https://rivet.dev/actors/docs/sqlite/
5. Rust API uses `create`, `on_create`, `on_start`, `on_before_connect`, `on_connect`, `on_subscribe`, `on_fetch_response`, `on_sleep`, `on_destroy`. These do not exactly match TS names in most conceptual docs. The trait has `CONCURRENT_HTTP_CALLBACKS=false` by default for raw HTTP; do not infer this serializes typed actions. https://github.com/rivet-dev/rivet/blob/v3.1.1/rivetkit-rust/packages/rivetkit/src/actor.rs
6. Rust transactions use `SqliteDbExt::transaction` and `SqliteTransactionOptions`. Commit on success, rollback on callback or commit error. Keep network/model/tool calls outside transactions. https://github.com/rivet-dev/rivet/blob/v3.1.1/rivetkit-rust/packages/rivetkit/src/sqlite.rs
7. Queues were deprecated in 3.0; Rust SDK has no queue API. Official replacement: pending work in SQLite, drain loop, keep-awake during work, restart drain on wake, idempotent effects. Add application retry deadlines and reconciliation because the example stops on failure until another enqueue/wake. https://rivet.dev/actors/docs/deprecated/queues/
8. One-shot and recurring schedules survive sleep/crash. Failed one-shots are completed after an attempted invocation; recurring failures continue at next cadence; overlaps skipped. Use timers as wakeups over durable task records, not as proof work succeeded. Rust `schedule().after/at/cancel` and `cron().set/every/history` verified in tagged context source. https://rivet.dev/actors/docs/schedule/ ; https://github.com/rivet-dev/rivet/blob/v3.1.1/rivetkit-rust/packages/rivetkit/src/context.rs
9. Active generation needs `keep_awake`, shutdown cancellation and durable checkpoints. Cleanup hooks can be skipped by crashes; graceful window is bounded. Actor wake should reconcile incomplete work. `wait_until` is not equivalent to preventing idle sleep. https://rivet.dev/actors/docs/lifecycle/
10. Events are a live delivery surface, not an application durable transcript. Design explicitly stored sequence-numbered output and cursor replay; reconnect alone cannot recover a missed chunk. A sent request may have an unknown outcome after disconnect; retries require request IDs. https://rivet.dev/actors/docs/events/ ; https://rivet.dev/actors/docs/clients/javascript/ ; https://rivet.dev/actors/docs/clients/rust/
11. Keep admin credentials server-side, issue scoped tokens if clients directly reach actors, and enforce domain authorization on action/event surfaces. Rust has `on_subscribe`; TS has `canSubscribe`. Compound actor keys are arrays. Do not treat caller-provided user IDs, roles, or sender strings in tutorial examples as authenticated identity. https://rivet.dev/actors/docs/authentication/ ; https://rivet.dev/actors/docs/permissions/ ; https://rivet.dev/actors/docs/keys/
12. Current docs list incoming message default 64KiB, outgoing 1MiB, hard frame/message 16MiB, action timeout default 60s, buffered HTTP response 16MiB. Streaming response can continue after starting; send SSE heartbeat for intermediary proxies. SQLite transaction cap now 32768 changed 4KiB pages = 128MiB. This is current docs, which can advance beyond release pins; verify deployed version rather than copying Layerr's old bound. https://rivet.dev/actors/docs/limits/
13. Destroy permanently deletes actor DB; `getOrCreate` can recreate empty actor at same key. Read/list routes should use get-existing and tombstone owner index entries to avoid accidental resurrection. https://rivet.dev/actors/docs/destroy/
14. Forks copy schedules and execution progress, so conversation branching must sanitize copied pending work and external effects in `on_fork`. Defer branching until this behavior is tested. https://rivet.dev/actors/docs/forking/

## Official examples worth using

- https://github.com/rivet-dev/rivet/tree/v3.1.1/examples/ai-agent-rust — exact starting point for Rust text generation, SQLite transcript and streamed events. `async-openai` helper with configurable API base URL.
- https://github.com/rivet-dev/rivet/tree/v3.1.1/examples/chat-room-rust — durable history versus transient `started_at_ms`, typed actions/events.
- https://github.com/rivet-dev/rivet/tree/v3.1.1/examples/hello-world-rust — minimal registry, client-visible typed actor, atomic SQL update.
- https://rivet.dev/actors/docs/deprecated/queues/#build-a-durable-job-queue — relevant durable pending-work pattern, conceptually port to Rust rather than using deprecated queue API.

The AI example is intentionally minimal: it stores the assistant message only at completion, emits full reply-so-far on every chunk, reloads the entire history, accepts overlapping generation, and has no auth, request deduplication, durable partial output, bounded context, tool loop or retry/reconciliation. Adopt its integration shape, not production guarantees. A crash mid-stream loses emitted partial output from history. Its hardcoded example model is not a model recommendation.

## Proposed application pattern

Conversation actor SQLite owns messages, runs, tool_calls, output_events and pending_jobs. `submit_turn(request_id)` atomically records input plus a queued run, then starts/wakes a drain. The drain acquires the run, commits intent, performs model/tool I/O outside a transaction, checkpoints output in bounded batches, and marks completion. Client delivery uses recorded event sequence plus live updates. User-memory actor owns explicit, provenance-bearing facts separately from conversational context. Rust provider/tool traits remain stateless modules, not actors per web search. Add a separate durable run actor later only if independent run lifetimes justify cross-actor coordination.
