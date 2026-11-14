# Turborepo and Cargo plan

Turborepo is the task coordinator; Cargo is the Rust dependency/build authority. Bun installs pinned Turbo tooling and runs repository scripts. JavaScript package manifests are tooling metadata; there is no JavaScript backend runtime.

## Proposed layout

```text
robot-buddy/
  package.json                  # private Bun workspace, pinned Turbo
  bun.lock
  turbo.json
  Cargo.toml                    # Cargo workspace
  Cargo.lock
  rust-toolchain.toml
  apps/
    server/                     # Rust Rivet registry plus narrow bootstrap HTTPS service
    robot-simulator/            # Rust integration client using official Rivet client
  crates/
    contracts/                  # actions/events/schema fixtures
    actors/                     # device, bot, conversation, memory, schedule
    agent/                      # bounded agent state machine
    model-provider/             # normalized model interface
    provider-openrouter/        # OpenRouter-specific routing/configuration
    provider-openai-compatible/ # local HTTP/SSE adapter
    tools/                      # registry, schemas, policy and execution
    robot-client/               # thin application wrapper over Rivet client
    observability/              # redacted tracing and usage metrics
  tooling/
    rust-workspace/              # one Turbo package wrapping workspace-wide Cargo tasks
  infra/                        # pinned local and deployment definitions
  docs/
    architecture/
    plans/
    research/
```

Create crates as their responsibilities become real; do not generate empty crates solely to match the diagram. Initially `server`, `robot-simulator`, `contracts`, `agent` and provider modules can form a compact workspace. Split adapters once tests justify their boundaries.

## Task strategy

Start with one Turbo workspace package wrapping the Cargo workspace. This avoids duplicating Cargo's dependency graph in dozens of package manifests and prevents concurrent workspace commands competing over the same target directory. Rust builds/checks/tests initially have Turbo caching disabled; Cargo performs incremental compilation. Add remote caching only with verified toolchain/OS/target hashes and restored outputs.

Planned package scripts under `tooling/rust-workspace/package.json`:

```json
{
  "name": "@robot-buddy/rust-workspace",
  "private": true,
  "scripts": {
    "build": "cargo build --workspace --locked",
    "check": "cargo check --workspace --all-targets --locked",
    "test": "cargo test --workspace --locked",
    "lint": "cargo clippy --workspace --all-targets --locked -- -D warnings",
    "format:check": "cargo fmt --all -- --check",
    "dev": "cargo run -p robot-buddy-server",
    "simulate": "cargo run -p robot-buddy-simulator"
  }
}
```

Cargo finds the workspace manifest in the ancestor root. These are planned commands; crate names and lockfiles will be created in phase 0.

Planned `turbo.json`:

```json
{
  "$schema": "https://turborepo.dev/schema.json",
  "globalDependencies": ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/**"],
  "tasks": {
    "build": { "cache": false },
    "check": { "cache": false },
    "test": { "cache": false },
    "lint": { "cache": false },
    "format:check": { "cache": false },
    "dev": { "cache": false, "persistent": true },
    "simulate": { "cache": false, "persistent": true }
  }
}
```

Supply explicit environment passthrough for `dev`/`simulate` at implementation time; Turbo strict environment filtering must not accidentally hide required Rivet/model settings. CI tests use fixtures and no production credentials. Integration tests requiring engine/network are separately named and never cached.

If individual app packages are later introduced, mirror their Rust source dependencies in Turbo's graph or explicit inputs. A changed shared Rust crate must invalidate every consuming build. Never assume Turbo infers Cargo dependencies.

## CI and releases

Run format, check, Clippy and unit tests; then engine/client integration tests with pinned versions. Run credentialed model conformance tests separately with capped spend. Build target-specific release binaries and container images, record checksums and SBOM, and preserve debug symbols for diagnosis. Keep hardware cross-compilation separate from server builds.

Do not initialize deployable manifests with floating dependency or image versions. Phase 0 produces pinned, tested files. No release or infrastructure changes are part of this planning deliverable.

Source: [Turborepo task configuration](https://turborepo.dev/docs/reference/configuration).
