---
title: Rivet documentation and ecosystem review
tags: [type/research, rivet, rust, agents]
---

# Rivet documentation and ecosystem review

Research date: 2026-11-13. This is evidence and design input, not a claim that example code has been compiled or adopted.

## Crawl method and boundaries

Started with the official [documentation index](https://rivet.dev/llms.txt), fetched all documentation, guides, ecosystem, integration and example pages, then recursively followed links in those Rivet path families. The JSON manifests record each URL and HTTP outcome. External source links were inventoried; directly linked Rivet-owned GitHub example files were also downloaded. Repository tree links and external documentation are listed but are not an unlimited recursive crawl of GitHub or the wider internet. Marketing, legal, social and dashboard links are excluded.

Retrieval is distinct from review. The focused review below reads architecture, deployment, authentication, versioning, agent/tool patterns, streaming and execution boundaries relevant to this backend. Individual generated API-schema pages, unrelated game examples and every source file are not claimed as manually reviewed. Actor/Rust findings are recorded separately.

Coverage: 455 index seeds expanded to 518 documentation URLs; 506 fetched successfully. Twelve failed after retry: four 404s and eight 403s in generated Sandbox Agent process schemas. All 598 directly linked Rivet-owned example source files fetched successfully; 1,119 external code/documentation links were catalogued. See `rivet-crawl-manifest.json` and `rivet-example-manifest.json` for exact URLs. The raw corpus is a research cache, not copied third-party code in this project.

## Applicable patterns

| Official source | Observed behavior | Application to Robot Buddy |
| --- | --- | --- |
| [Architecture](https://rivet.dev/docs/architecture/) | Nodes run application code and connect outward to the control plane; the control plane routes actors and owns storage tiers. | Robot clients connect to the Rivet endpoint; actor nodes need no separate public actor-traffic ingress. Keep infrastructure and application storage responsibilities distinct. |
| [Deployment](https://rivet.dev/docs/deploy/) | Managed, own-node, BYOC and self-host options exist. | Make hosting a deployment choice without changing the Rust domain code. Review specific self-host storage docs rather than copying older diagrams. |
| [Scoped tokens](https://rivet.dev/docs/jwt/) and [authentication](https://rivet.dev/docs/authentication/) | Gateway access and actor management are distinct grants; admin credentials remain server-side. | Pair the robot with a device identity, issue short-lived tokens scoped to resolved actor IDs, and authorize actions inside actors. A gateway grant alone is not user/device authorization. |
| [Versions](https://rivet.dev/docs/versions/) | Node versions influence placement; running old actors remain until stopped/drained. Self-hosted control plane upgrades precede SDK upgrades. | Use explicit deployment versions, compatible schema migrations and drain tests. Avoid assuming every actor switches at one instant. |
| [Agent design patterns](https://rivet.dev/guides/agent-design-patterns/) | Examples distinguish per-key agents, background submissions, tenant credentials and repeat request IDs. | Keep stable conversation identity, scope credentials by owner, and persist accepted input before returning. These are patterns to implement in Rust, not permission to import TypeScript harness code. |
| [Custom tools](https://rivet.dev/guides/agent-custom-tools/) | The model receives schemas/results; credentials stay with backend tool code. Tools distinguish safe replay from effects with uncertain completion. | Validate tool arguments, propagate cancellation, redact results, and record effect state before external calls. Treat uncertainty explicitly rather than replaying a physical or external mutation blindly. |
| [Human approval](https://rivet.dev/guides/agent-human-in-the-loop/) | The example binds approval to a concrete operation and consumes it once. | If future integrations require approval, bind it to owner, action, exact arguments and expiry. Implement authorization and atomic consumption; sample snippets are not a complete security layer. |
| [System prompts](https://rivet.dev/guides/agent-system-prompt/) | Context is assembled from scoped sections and prompt changes become history. | Record model configuration and prompt revision per run; treat retrieved web text as data and keep user memory separate from global instructions. |

## Libraries versus concepts

**Pi Durable:** The [integration](https://rivet.dev/ecosystem/pi-durable/) installs `@rivet-dev/pi` and a TypeScript agent harness. Its examples provide useful semantics for accepted submissions, tool replay, context, cancellation and durable conversations, but are not a native Rust harness implementation. For the pure Rust requirement, use the Rust actor SDK and implement a bounded, persisted agent loop. Do not claim Pi's recovery guarantees automatically apply to that new loop.

**Rivet Workflows:** The [workflow examples](https://rivet.dev/ecosystem/rivet-workflows/) use `@rivet-dev/workflows`. They illustrate durable steps, waits, retries and compensation, but do not establish Rust workflow API parity. Use a Rust state machine with durable scheduling and operation IDs until a supported Rust workflow API is independently verified.

**Durable Streams:** The [stream documentation](https://rivet.dev/ecosystem/durable-streams/) describes a language-neutral HTTP protocol, durable offsets and idempotent producers. It is a candidate for a later external event-log service, but requires its own integration and authorization. The initial robot contract can stay entirely on the requested Rivet client: actor events for live delivery and an actor action for persisted event replay. Do not equate an ordinary live event with a durable stream.

**Hosted agentOS:** The [hosted API](https://rivet.dev/agentos/docs/hosted-api/) is callable over HTTP, so a Rust backend could use it without a JavaScript host. Durable configuration/files and transient processes have different lifetimes; generation-scoped handles prevent stale operations. The [persistence guide](https://rivet.dev/agentos/docs/persistence/) requires an explicitly durable root filesystem. This is optional for later shell/browser tools; web search and memory do not require a VM.

**Secure Exec:** The [security guide](https://rivet.dev/secure-exec/docs/security/) describes a sidecar isolate and explicit host-function/network boundaries. Its TypeScript embedding examples are not a required Rust dependency. Untrusted execution should remain a separate tool service if added.

**Sandbox Agent:** The [orchestration guide](https://rivet.dev/sandbox-agent/docs/orchestration-architecture/) emphasizes backend-owned session state and recovery. Its [persistence examples](https://rivet.dev/sandbox-agent/docs/session-persistence/) configure TypeScript drivers and otherwise use memory. It orchestrates coding-agent processes; it is not needed for the core OpenRouter/local-model assistant.

## Design implications

1. Keep one authoritative transcript owner and persist run/effect status alongside it initially; split actors only where isolation or independent lifecycle earns the cross-actor coordination cost.
2. Separate accepted input, provider attempt, completed model message, tool intent, tool result and published event. A crash between any two must have an explicit recovery rule.
3. Use client request IDs and durable event cursors. Reconnection, request deduplication and external-effect deduplication solve different problems.
4. Keep OpenRouter/local-model adapters behind a model interface; tool authorization belongs outside model output.
5. Verify native Rust SDK behavior with crash, cancel, reconnect, timer and migration tests before treating the architecture as implemented.

## Source-example checks

Directly read the source files for the [token endpoint](https://github.com/rivet-dev/agents/blob/main/examples/docs/client-sdk/token.ts), [custom tool](https://github.com/rivet-dev/agents/blob/main/examples/docs/custom-tools/get-order.ts), [background-job submission](https://github.com/rivet-dev/agents/blob/main/examples/docs/design-patterns/background-job/client.ts), [background-job tool](https://github.com/rivet-dev/agents/blob/main/examples/docs/design-patterns/background-job/server.ts), and [approval tool](https://github.com/rivet-dev/agents/blob/main/examples/docs/human-in-the-loop/deploy.ts). They corroborate the patterns above. The token example explicitly contains placeholder authentication reading a user-ID header; this must be replaced with verified device/user credentials. The approval example is illustrative and needs authorization and durable effect reconciliation before production use. None was executed as part of this documentation review.
