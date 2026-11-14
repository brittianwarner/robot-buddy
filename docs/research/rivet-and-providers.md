---
title: Robot Buddy runtime and provider research
tags: [type/research, rivet, rust, ai]
---

# Research overview

Research date: November 13, 2026. These are source-backed findings; application design is in the architecture documents. No downloaded example was compiled or executed during planning.

## Rivet baseline

Live release lookup identified [v3.1.1](https://github.com/rivet-dev/rivet/releases/tag/v3.1.1), published November 11, 2026. Search caches showed older versions, so do not use search snippets as the release authority. Rust actor support is [generally available](https://rivet.dev/changelog/2026-11-06-rust-sdk-for-rivet-actors-is-now-generally-available/). The [Rust quickstart](https://rivet.dev/actors/docs/quickstart/rust/) demonstrates native actors, SQLite and a typed client; a custom runner implementation is unnecessary.

Use [actor research](rivet-actor-findings.md) for detailed runtime evidence and [ecosystem research](rivet-ecosystem-review.md) for agent patterns, deployment, workflows and sandbox applicability. The important distinction is native Rust actors versus TypeScript agent harness/workflow integrations: the latter do not automatically provide a Rust recovery implementation.

## OpenRouter and local inference

[OpenRouter's API](https://openrouter.ai/docs/api/reference/overview) supports Chat Completions and Responses with a normalized model/provider interface. Its [streaming documentation](https://openrouter.ai/docs/api/reference/streaming) describes incremental responses; the backend must handle completion and error semantics, not only text chunks.

[Ollama's compatibility documentation](https://docs.ollama.com/api/openai-compatibility) explicitly describes a subset of the OpenAI API and a local `/v1` endpoint. This supports using a configurable OpenAI-compatible adapter, but does not prove that every model supports tools, images or identical structured output. Capability conformance tests are required for each deployed model/server combination.

Design recommendation: Rust HTTP/SSE adapter with OpenRouter defaults, provider-specific fields isolated, configurable private local endpoint, explicit model capabilities and no implicit cloud fallback for local-only policy. Search and memory are backend tools independent of model vendor. No model name or performance is selected solely from documentation examples.

## Repository tooling

[Turborepo task configuration](https://turborepo.dev/docs/reference/configuration) uses package scripts and explicit cache/input/output settings. A thin tooling package can invoke Cargo workspace commands. Cargo remains the dependency authority; initial Turbo cache is disabled for Rust commands until invalidation/restoration is proven.

[Axum](https://docs.rs/axum/latest/axum/) is a candidate for the narrow Rust bootstrap HTTPS service; [Reqwest](https://docs.rs/reqwest/latest/reqwest/) is a candidate for pooled outbound HTTP. Their exact versions are selected and locked with the compatibility spike.

## Crawl coverage

- [General crawl manifest](rivet-crawl-manifest.json): 518 in-scope URLs attempted, 506 successfully fetched; failures retained after retry.
- [Example-link manifest](rivet-example-manifest.json): 598 directly linked Rivet-owned source example files fetched, plus catalogued external links.
- [Actor manifest](actors-manifest.json): 54 indexed actor pages fetched, with 428 fenced code blocks; four additional stale actor Markdown endpoints recorded. These overlap the general crawl and must not be added to its page count.
- [Pinned Rust source manifest](rust-source-manifest.json): 18 files from all three Rust actor examples and relevant SDK source at v3.1.1.

The crawl covered Rivet docs, guides, ecosystem, integrations and examples, including recursive links in those families. It catalogued external links without recursively crawling the internet. Four general-crawl URLs returned 404, and eight generated Sandbox Agent process-schema URLs returned 403. All failures are listed in the manifest.

All three Rust actor examples and critical actor/agent documentation received focused review. Downloading all examples does not mean all 598 files were manually audited; unrelated/generated examples were indexed. Full downloaded source is retained only in the temporary research cache, not vendored into this project. This plan cites reusable sources instead of copying their implementations.
