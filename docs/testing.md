# Testing

What each suite proves, how to run it, and what none of them prove. Run everything from
the repository root with Bun (`bun install` first). Results from a dated run are in
[validation](research/validation.md); latency numbers are in [e2e latency](research/e2e-latency.md).

## Suites

| Command | Runs in CI | Needs | Proves |
| --- | --- | --- | --- |
| `bun run format:check`, `bun run check`, `bun run lint` | yes | Rust toolchain | Formatting, type check of every target, Clippy with `-D warnings`. |
| `bun run lint:pedantic` | no | Rust toolchain | Clippy pedantic (with the four noisy lints allowed). Test files under `crates/buddy/tests` are clean; source files still carry warnings, so it is not a gate yet. |
| `bun run test` | yes | Rust toolchain | Unit tests plus the offline integration-test crates below. Tests that need an engine are `#[ignore]`d here. |
| `bun run test:integration` | yes | Network once (engine download), free ports | The 17 actor scenarios against a real Rivet engine. |
| `cargo build --release`, `cargo bench --no-run` | yes | Rust toolchain | Release builds link; benches compile. |
| `bun tests/early-player.mjs` | yes | Chromium (`bunx playwright install chromium`) | The lab voice player plays streamed PCM and stops on demand. Offline and deterministic. |
| `bun run deny` | yes (`cargo-deny-action`) | `cargo install cargo-deny` | Advisories, licenses, crate sources and bans per `deny.toml`. |
| robot-aarch64 job | yes | `gcc-aarch64-linux-gnu` | The robot client type-checks for the Raspberry Pi 5 target. |
| `bun run bench:e2e` | no (opt-in) | Release build, free ports, port 8090 | End-to-end latency with the real server, engine and robot client. See below. |
| `bun run test:container` | no | Docker | Enrollment and stored memory across a container replacement. |
| `bun run test:lab`, `tests/early-live.mjs`, `tests/latency-live.mjs` | no | A running lab, `BUDDY_LAB_TOKEN`, a device identity; the live scripts also need a provider key | Browser acceptance of the lab. See [lab](lab.md). |
| `bun tests/robot-shutdown.mjs` | no | Release build of server and robot | The robot stops on SIGTERM while stdin is open. Currently fails (see Known bugs). |

### Offline test crates (`crates/buddy/tests`)

- `protocol_golden.rs` pins the wire shape of every `Command` variant (an exhaustive match
  fails to compile when a variant is added without a fixture), the action envelope,
  connection params, output events, enrollment messages and settings payloads against
  JSON in `fixtures/protocol/`. It also checks that clients ignore additive fields on
  messages they read and that unknown commands are rejected. After a deliberate wire
  change, regenerate with `BUDDY_UPDATE_GOLDEN=1 cargo test -p robot-buddy --test protocol_golden`
  and review the fixture diff.
- `voice_rules.rs` runs the table in `fixtures/voice_rules.json` (34 cases) through
  `VoiceConfig::speech_input` and `speech_body`: styles, word boundaries, case, overlap,
  validation errors, the 32-rule cap and the 24 KiB rendered-input cap. Rendering may only
  add `[whispering] `; the canonical transcript is never changed.
- `sentences.rs` and `history_sql.rs` cover the sentence splitter and the history queries.

### Integration harness (`crates/buddy/tests/actors.rs`, `tests/support/mod.rs`)

`bun run test:integration` (`tests/integration.mjs`) sets test-only credentials, starts
`cargo test -p robot-buddy --test actors -- --ignored`, and afterwards fails if anything
still listens on its ports or a run directory is left behind.

- One engine per test binary, spawned and owned by the harness, with storage in
  `$TMPDIR/robot-buddy-it/run-<pid>-<millis>`. rivetkit never spawns or reuses an engine
  (`EngineSpawnMode::Never`).
- Ready means: engine `/health` reports the engine runtime, the registry readiness probe passes, and a
  real `Bot` enroll action round-trips.
- A `sh` watchdog holds the test binary's stdin. When the binary dies, even by SIGKILL, it
  stops the engine and removes the run directory. macOS has no parent-death signal, so this
  replaces one.
- At start the harness reaps engines from earlier runs whose owner is dead (matched by
  storage path, parent PID 1) and says which. Any other listener on its ports is reported
  with its PID and command, and the run refuses to start.
- Tests share one engine behind a read/write gate. Latency, barge-in and restart tests take
  it exclusively so their timings are not shared with other tests.
- The model is an axum mock on `BUDDY_TEST_MODEL_PORT`. Web search goes to a fake Brave key
  through `HTTPS_PROXY` pointing at the mock, which accepts the CONNECT and never answers:
  a deterministic tool that is still in flight when the engine restarts.
- Event subscriptions replay from the first hole in the sequence, so an event dropped while a
  socket reconnects is still seen. Pushed events are stamped when the push arrives.
- The search client gives the proxy tunnel 5 s. If the graceful stop takes longer than that to reach
  the tool's actor, the search fails by itself before the restart and the checkpoint is never
  exercised. The restart test detects this (a `turn.failed` pushed before the restart finished)
  and retries the tool leg once.

Scenarios: enrollment idempotency and session binding; single-use challenges and one-time
claim confirmation; revocation across bot, conversation and memory; one provider call per
chat turn with private text hidden; tool turns that write memory then speak; direct-mode
tool call after spoken text fails the turn; one numbered speech chunk per sentence;
barge-in closes the provider stream and commits nothing after; a running turn pins its
voice, rejects a second submit and cancels cleanly; bot isolation; compare-and-swap on
voice and context settings; system-prompt tool writes apply to the next turn; persona
save, activate and roll back; persona library limits (50 names, 16 versions); history
paging and phrase search; memory remember, search and forget; engine restart interrupts
live streams and fails closed on unresolved tools.

## Ports

| User | Ports |
| --- | --- |
| Integration engine | `BUDDY_TEST_ENGINE_PORT` (default 16420), +1 api peer, +10 metrics |
| Integration model mock | `BUDDY_TEST_MODEL_PORT` (default 19091) |
| E2E bench | `BENCH_PORT_BASE` (default 17500): engine +20 (+21, +30), server +80, model +91, speech +92 |
| Robot loopback panel | 8090 (fixed in `apps/robot/src/screen.rs`) |
| Local lab compose | 18080, 17420 |

The integration run and the bench use different ports and can run side by side; two
benches cannot, because the robot panel port is fixed.

## E2E latency bench

`bun run bench:e2e` builds release binaries, then runs the real `robot-buddy-server` (it
spawns its own engine on isolated ports), enrolls and owner-claims a device through the
server and the Rivet gateway, and starts the real `robot-buddy-robot --simulate` with
`BUDDY_SPEECH=true`. Utterances go to the robot's stdin; its event lines come back on stdout.

- Fake model: SSE, first token at 250 ms, then 4 characters every 15 ms; replies of 2 to 3
  sentences (12 for barge-in turns).
- Fake speech (`TTS_BASE_URL`): `POST /v1/audio/speech` returns `audio/pcm` after 300 ms, then
  24 kHz mono s16 in real time (48 kB/s), 60 ms of audio per input character.
- Fake `aplay` on `PATH` (`tests/lib/fake-aplay.mjs`): timestamps every PCM chunk, keeps
  playing its buffer after stdin closes like aplay, and its socket closing marks the kill.

Measures: submit to `turn.started`, first `text.delta`, first `speech.ready`, first PCM byte
at the player, `turn.completed`; silence inside a reply (a chunk arriving after the buffered
audio ran out, 20 ms floor); barge-in to player killed, to the turn's end event and to the
model stream closing; server and engine RSS. Percentiles are nearest-rank, so with 20 turns
p95 is the 19th value and p99 is the maximum.

Options: `--turns N` (20), `--barge N` (5), `--out file.json`, `--no-build`, `--no-speech`,
`--keep` (keep the work directory), `--repo DIR` and `--label NAME` to measure another
checkout. A server without `TTS_BASE_URL` is never given `TTS_MODEL`, so it cannot reach the
real provider; only text and `speech.ready` are measured there. The bench refuses to start
if a port is busy and exits non-zero if a turn fails or the server, engine or a listener
outlives shutdown. Credentials are generated per run in a temporary directory and deleted.

## Known bugs pinned by tests

- `voice_rules.rs::longest_overlapping_match_wins_across_start_positions` (ignored):
  `speech_input` resolves overlapping rules leftmost-first, not longest-first as
  `docs/architecture/voice.md` says. "New York" (emphasis) beats the longer overlapping
  "York City Hall" (whisper).
- `voice_rules.rs::model_authored_tags_do_not_reach_the_provider` (ignored): model-authored
  bracket tags such as `[laughs]` reach the Eleven v4 input unchanged; only `[whispering]`
  from saved rules should.
- `protocol_golden.rs::settings_payloads_reject_unknown_fields` documents, as passing
  behavior, that `VoiceConfig`, `ContextConfig`, `Synthesis`, `PersonaDraft` and `SpeechRule`
  reject unknown fields, which contradicts "Clients ignore additive unknown fields"
  (`docs/architecture/robot-client.md`). Decide which is intended.
- `tests/robot-shutdown.mjs` (fails): the robot does not exit on SIGTERM while stdin is open.
  The graceful-shutdown handler returns from `main`, but dropping the Tokio runtime waits on
  the blocking `tokio::io::stdin` read, which never returns. Baseline 983e90f had no handler
  and died at once. On the device systemd waits `TimeoutStopSec`, then SIGKILLs.

Ignored BUG tests run with `cargo test -p robot-buddy --test voice_rules -- --include-ignored`
and fail until fixed; remove the `#[ignore]` with the fix.

## Not proven here

- Real providers: model quality, tool support of a local model, OpenRouter speech latency
  and audio quality. The fakes fix those latencies, so the bench measures our overhead only.
- Robot hardware: ALSA output, the ESP32 face, NetworkManager setup, wake word and speech
  recognition. `--simulate` skips Wi-Fi and the fake player has no real buffer underruns.
- aarch64 runtime: CI type-checks the robot for the Pi; nothing runs on one.
- Real OIDC login (only the controlled-provider lab test and the unit test cover it),
  production load, multi-node engines, backup and restore.
- Timings come from a shared developer machine; run the bench on idle hardware before
  comparing small differences.
