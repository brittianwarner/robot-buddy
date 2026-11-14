---
title: Rig integration findings
tags: [type/research, rust, agents]
---

Research date: 2026-11-13.

## Verified release and documentation drift

Latest GitHub release is **v0.46.0**, published 2026-10-31. The published API reference also identifies 0.46.0. Depend on the facade `rig = "=0.46.0"`; `rig-core` alone no longer includes the agent runtime. Tagged Cargo requires Rust 1.97.1. Website installation examples still show 0.44.0: use tagged source for implementation.

Sources: [release](https://github.com/0xPlaygrounds/rig/releases/tag/v0.46.0), [tagged Cargo](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/Cargo.toml), [published API](https://docs.rs/rig/0.46.0/rig/).

## Integration surface

The best matching upstream runnable example is [agent_run_stepping](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/examples/agent_run_stepping/src/main.rs). `AgentRun::new(prompt).with_history(messages).max_turns(n)` owns loop progress. `next_step()` yields CallModel, CallTools or Done. The state serializes with serde; save and reload while tools are pending, and `next_step()` re-emits those pending calls. Match `PendingToolCall::Execute(call)` and `PendingToolCall::Malformed(call)`, then use `run.answer(call.answer(result))`; the website's older `tool_results` and `preresolved_result` sample is stale.

A completed model response enters the state machine as:

```rust
let mut outcome = run.model_response(ModelTurn::new(
    response.head(), response.choice.clone(), response.usage,
    TurnPolicy::new(tool_names, None, None)?, response.raw.clone(),
))?;
while let ModelTurnOutcome::NeedsResolution(_) = outcome {
    outcome = run.resolve_invalid_tool_call(InvalidToolCallAction::fail())?;
}
```

The exact streaming driver is the function `run_streamed_turn` in [agent_run_streamed.rs](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-cassette/tests/providers/gemini/cassette/agent_run_streamed.rs). Its protocol is provider independent: `model.stream(request)?`, `StreamedTurnAssembler::new(policy)`, `assembler.ingest(&item)`, handle invalid calls, `stream.finish().await`, `run.record_streamed_completion_call(...)`, and `run.streamed_turn(assembler.finish(&response))`. Text appears as `Item::Event(StreamEvent::Text { text, .. })`. Do not invent another SSE decoder or tool-call assembler. The simpler alternative is to stream provider events and finish into the ordinary response consumed by `model_response`; invalid-call handling then happens after completion.

## Hooks and cancellation

[Tagged hook source](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-agent/src/agent/hook.rs) defines async `AgentHook::on_dispatch(&self, &HookContext, DispatchEvent<'_>) -> DispatchAction` before effects and `on_outcome(..., OutcomeEvent<'_>) -> OutcomeAction` afterward. Dispatch has effect id, turn and optional call id; helpers expose tool name/arguments. Outcome exposes the matching identity and result. Await durable persistence before returning proceed; stop on failed persistence. Hook cancellation is available through dispatch stop/deny actions; cancellation does not undo an effect that has already happened. Hooks do not by themselves provide a serialized whole-run checkpoint at each boundary.

High-level [AgentRunner](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-agent/src/agent/streaming.rs) supports `agent.prompt(prompt).history(history).max_turns(n).stream()` and `agent.resume(run)`. Stream text is `MultiTurnStreamItem::StreamAssistantItem(Item::Event(StreamEvent::Text { text, .. }))`. ToolCall is transcript commitment, not execution; execution has a separate lifecycle event. FinalResponse contains committed messages and usage. Dropping a stream stops its driver, but cannot prove an external operation did not happen.

## Provider configuration

The [OpenRouter module](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-core/src/providers/openrouter/mod.rs) builds the OpenAI-shaped OPENROUTER dialect; `openrouter::from_env()?.chat(model_id)` is directly demonstrated. Configurable endpoint construction is exposed through `rig::providers::openai::OpenAIConfig`, `with_key(&dialect, key)`, `with_base_url(url)`, and `.client().chat(model_id)`; use OPENROUTER for hosted routing, an appropriate OpenAI-compatible dialect for local chat endpoints. Explicit `.chat()` avoids accidentally selecting the Responses API. [Local endpoint documentation](https://rig.rs/docs/integrations/model_providers/local_models).

## Actor integration assessment

Inference from these APIs: hand-step AgentRun inside the Conversation actor's turn executor. It replaces the custom agent state machine while keeping the actor as durable owner. Save a versioned run snapshot and effect intent before dispatch; persist each result before advancing the run. Keep an independent effect ledger keyed by application turn and tool-call identity; a pending intent without result after crash is uncertain, not safe to execute blindly. Memory actor writes can accept idempotency keys. Search can explicitly allow retry. Physical robot actions need their own durable acknowledgement protocol.

Do not serialize an entire conversation on each token. Keep canonical transcript separately; supply a bounded working context through with_history and history search tools. Checkpoint on accepted model turns and each tool transition; stream events can be batched with sequence ids. Pin run format and library version; unknown checkpoint formats must fail explicitly or migrate.

Validation: official tagged source/API reviewed. Upstream runnable and test source is linked above; this research did not compile or execute Rig, call a model, or test Rivet integration. Local source copies for implementation inspection are in `/private/tmp/rig-src/` (0.rs runnable stepping example, 1.rs streamed driver test, hook.rs hooks, run.rs state machine).

Additional source check: `AgentRun::next_step` reissues CallModel from persisted AwaitingModel without consuming another turn, so checkpointing after obtaining the step and before dispatch works. Recovery retry counts therefore need an application limit independent of max_turns. See [run.rs](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-agent/src/run/mod.rs). Endpoint construction was additionally checked in [OpenAIConfig](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-core/src/providers/openai/wire.rs) and [OpenAI client](https://github.com/0xPlaygrounds/rig/blob/v0.46.0/crates/rig-core/src/client/openai.rs): `OpenAIConfig::with_key(&OPENROUTER, key).with_base_url(base).client().chat(model_id)`; use OPENAI for a generic local endpoint.
