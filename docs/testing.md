# Testing

What each suite proves, how to run it, and what none of them prove. Run everything from
the repository root with Bun (`bun install` first). Results from a dated run are in
[validation](research/validation.md); latency numbers are in [e2e latency](research/e2e-latency.md).

`bun run check:all` (`scripts/check-all.mjs`) runs the whole local gate in CI order and
stops at the first failure: fmt, Clippy `-D warnings`, `cargo test`, the robot feature
profile, `deny`, `test:integration`, a release build, `test:shutdown`,
`tests/robot-reconnect.mjs --no-build --strict`, `cargo bench --no-run`,
`tests/early-player.mjs`, `test:lab-reconnect`, `eval:voice` and `bench:report:check`. It
passes its environment to every step, so set the port variables (see [ports](#ports)) and
`BUDDY_STRICT_TIMING` once in front of it, and it ends with a table of every step's result
and wall time. `-- --from <step>` resumes at a step.

## Suites

| Command | Runs in CI | Needs | Proves |
| --- | --- | --- | --- |
| `bun run format:check`, `bun run check`, `bun run lint` | yes | Rust toolchain | Formatting, type check of every target, Clippy with `-D warnings`. |
| `bun run lint:pedantic` | via `lint` | Rust toolchain | Clippy pedantic with the four noisy lints allowed. `[workspace.lints.clippy]` already enables the same set, so `bun run lint` (`-D warnings`) gates it; this script only spells it out. |
| `bun run test` | yes | Rust toolchain | Unit tests plus the offline integration-test crates below. Tests that need an engine are `#[ignore]`d here. |
| `bun run test:robot-features` | yes | Rust toolchain | `cargo test -p robot-buddy --no-default-features --locked`: the shared crate's tests in the feature profile the robot links (no actor runtime, no Rig). |
| `bun run test:integration` | yes | Network once (engine download), free ports | Four engine-backed test binaries, one after another: the actor scenarios (`actors`), plus `core_sql_counts`, `core_retention` and `core_logs`, against a real Rivet engine, including output-event fixtures captured from the actor and the version 1 history upgrade. Ends with each suite's wall time. Timing limits are reported, not enforced (see [timing limits](#timing-limits)). |
| `cargo build --release`, `cargo bench --no-run` | yes | Rust toolchain | Release builds link; the three benches in `crates/buddy/Cargo.toml` compile (`autobenches = false`, so `benches/legacy_sentences.rs` is only a module of `sentences`). |
| `bun tests/early-player.mjs` | yes | Chromium (`bunx playwright install chromium`) | The lab voice player plays streamed PCM and stops on demand. Offline and deterministic. |
| `bun run deny` | yes (`cargo-deny-action`) | `cargo install cargo-deny` | Advisories, licenses, crate sources and bans per `deny.toml` (`cargo deny --locked --all-features check`, the same arguments locally and in CI). Wildcard versions fail: every workspace crate is `publish = false`, so only its path dependencies are exempt. Duplicate major versions warn; `deny.toml` names the crate forcing each one. |
| robot-aarch64 job | yes | `gcc-aarch64-linux-gnu` | The robot client type-checks for the Raspberry Pi 5 target. |
| `bun run bench:e2e` | no (opt-in) | Release build, free ports | End-to-end latency with the real server, engine and robot client, and the audio checks below. See below. |
| `bun run bench:report` | no | `docs/research/e2e-latency.json` | `bun tests/e2e-report.mjs --write`: rewrites the results table in [e2e latency](research/e2e-latency.md) from the published raw JSON. |
| `bun run bench:report:check` | yes (`bench-artifact` job, full history) | git history back to the measured commit | `bun tests/e2e-report.mjs --check`: fails when the doc's table differs from the JSON, or the JSON does not describe HEAD (see [published artifact](#published-artifact)). |
| `bun run test:reconnect` | yes, after the release build (`--no-build --strict`) | Release build, free ports | The real robot binary through a fault proxy: interruption during a held replay, reconnect after a lost submit ack, no old speech after it. See below. |
| `bun run test:lab-reconnect` | yes | Chromium (`bunx playwright install chromium`) | `apps/server/tests/lab-reconnect.mjs`: the real lab page against a mocked lab API. A reply that completes while the lab is disconnected is not spoken on reconnect; Replay still speaks it. Offline and deterministic. |
| `bun run eval:voice` | no (opt-in) | Release build, free ports | A JSON scorecard of six voice-turn metrics against `tests/eval/voice-thresholds.json`; time limits enforced only under strict timing. See below. |
| `bun run test:container` | no | Docker | Enrollment and stored memory across a container replacement. |
| `bun run test:lab`, `tests/early-live.mjs`, `tests/latency-live.mjs` | no | A running lab, `BUDDY_LAB_TOKEN`, a device identity; the live scripts also need a provider key | Browser acceptance of the lab. See [lab](lab.md). |
| `bun run test:shutdown` | yes, after the release build | Release build of server and robot | The robot exits 0 on its own after SIGTERM or SIGINT with stdin open or closed (before the script's 5 s SIGKILL), and writes its cache file on the way out; the 500 ms limit is printed as `TIMING robot_stop_ms` and enforced under `BUDDY_STRICT_TIMING=1` (3 SIGTERM, 1 SIGINT, 1 closed-stdin run). Each signal is sent after the robot's first connection to its server URL. The stop handlers are registered first thing in startup, so an earlier signal is a clean stop too. The robot runs with its panel off (`BUDDY_SCREEN_ADDR=off`). |

### Offline test crates (`crates/buddy/tests`)

- `protocol_golden.rs` pins the wire shape of every `Command` variant (an exhaustive match
  fails to compile when a variant is added without a fixture), the action envelope,
  connection params, output events, enrollment messages and settings payloads against
  JSON in `fixtures/protocol/`. It records the compatibility rule: every message and every
  settings type read back (voice, context, synthesis, speech rule, persona draft) ignores
  additive fields, while settings writes (the config in `set_voice_config` and
  `set_context_config`, the persona in `save_persona`, and model tool arguments, all through
  `protocol::strict`) reject an unknown field at any depth and name it. Unknown commands
  are rejected. After a deliberate wire
  change, regenerate with `BUDDY_UPDATE_GOLDEN=1 cargo test -p robot-buddy --test protocol_golden`
  and review the fixture diff.
- Output events are not built in that file. Their fixtures (`output_event.*.json`, one per
  kind: `turn.started`, `text.delta`, `speech.ready`, `tool.started`, `tool.completed`,
  `turn.completed`, `turn.cancelled`, `turn.failed`, `turn.interrupted`) are captured from
  real turns by `actors.rs::output_events_match_their_golden_fixtures` and the restart test,
  which check that each pushed event equals its replayed copy and that both equal the fixture
  once sequence and turn ID are normalized. `protocol_golden.rs` then checks the set is
  complete, each round-trips and ignores additive fields, and `speech.ready` carries only
  `index`. Recapture with `BUDDY_UPDATE_GOLDEN=1 bun run test:integration` and review the diff.
- `voice_rules.rs` runs the table in `fixtures/voice_rules.json` (34 cases) through
  `VoiceConfig::speech_input` and `speech_body`, and through `VoiceConfig::for_speech`, which
  validates once per stream and then renders each sentence with the same output and errors:
  styles, word boundaries, case, overlap,
  validation errors, the 32-rule cap and the 24 KiB rendered-input cap. Overlaps resolve
  longest first across start positions, ties leftmost. Model-authored markup (`[laughs]`,
  `<break/>`, a stray `[whispering]`) is removed, so rule-derived `[whispering] ` is the
  only tag that reaches the provider; the canonical transcript is never changed.
- `sentences.rs` and `history_sql.rs` cover the sentence splitter and the history queries.
  `history_sql.rs` checks exact search results on both readers at every batch boundary and
  measures the startup DDL apart from the rebuild start transaction and its batches (WAL
  pages per commit, numbers in [benchmarks](research/benchmarks.md)).

### Integration harness (`crates/buddy/tests/actors.rs`, `tests/support/mod.rs`)

`bun run test:integration` (`tests/integration.mjs`) sets test-only credentials and runs
`cargo test -p robot-buddy --test <suite> -- --ignored` for `actors`, `core_sql_counts`,
`core_retention` and `core_logs`, one at a time: each binary owns one engine on the same
ports. After each it waits up to 20 s for its ports to close and its run directories to go,
and fails (stopping there) if anything still listens or a run directory is left behind.
Extra arguments go to every suite.

- One engine per test binary, spawned and owned by the harness, with storage in
  `$TMPDIR/robot-buddy-it/engine-<port>/run-<pid>-<millis>` through `test:integration`
  (`BUDDY_TEST_STATE_DIR` overrides it). One directory per engine port keeps checkouts
  running side by side on different ports from reporting each other's run directories
  as leaks; a bare `cargo test --test actors -- --ignored` uses `$TMPDIR/robot-buddy-it`. rivetkit never spawns or reuses an engine
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
- The model is an axum mock on `BUDDY_TEST_MODEL_PORT`. The restart test holds
  `memory_remember` through a deterministic test seam rather than a proxy or retry, so the
  effect is guaranteed to be in flight when the engine restarts.
- Event subscriptions replay from the first hole in the sequence, so an event dropped while a
  socket reconnects is still seen. Replay pages are capped by count and at 512 KiB, so a pull
  pages until a page comes back empty. Pushed events are stamped when the push arrives.

Scenarios: enrollment idempotency and session binding; single-use challenges and one-time
claim confirmation; revocation across bot, conversation and memory; one provider call per
chat turn with private text hidden; tool turns that write memory then speak; direct-mode
tool call after spoken text fails the turn; one numbered speech chunk per sentence;
barge-in closes the provider stream and commits nothing after; cancel by request ID
(`CancelRequest`, including before the submit arrives); spoken-phase cancel; web-result
taint refusing subsequent write and history tools; short-lifetime retention; a running turn
pins its voice, rejects a second submit and cancels cleanly; bot isolation; compare-and-swap
on voice and context settings; system-prompt tool writes apply to the next turn; persona
save, activate and roll back; persona library limits (50 names, 16 versions); history
paging and phrase search; memory remember, search and forget; engine restart interrupts
live streams and fails closed on unresolved tools; every output event kind matches its
captured fixture; a version 1 history database upgraded by the actual actor on the
engine, stopped mid-rebuild at least once by swapping the registry (`Harness::swap_registry`)
to a stand-in actor that inspects the database, with exact search results at every stop.

### Timing limits

Wall-clock limits in the integration suite (submit to first `speech.ready` under 1500 ms,
provider stream closed under 300 ms after a cancel) go through `support::timing_bound`. It
always prints `TIMING <name>_ms=<took> limit_ms=<limit> ok|OVER`, and fails the test only
when `BUDDY_STRICT_TIMING=1`. Set that on a controlled runner (idle, dedicated hardware).
CI runners are shared, so CI reports the timings and enforces only the functional
assertions, which always run: speech chunks numbered in order, zero events committed after
`turn.cancelled`, the provider connection dropped within 5 s, and so on.

The JS harnesses follow the same rule through `tests/lib/timing.mjs` (`timingBound`, same
`TIMING` line):

| Harness | Always enforced | Only under `BUDDY_STRICT_TIMING=1` |
| --- | --- | --- |
| `tests/robot-shutdown.mjs` | exit 0 on its own, cache flushed | stop within 500 ms |
| `tests/robot-reconnect.mjs` | B's player stops before the held replay is released | wake → player stop within 1000 ms |
| `tests/eval-voice.mjs` | functional checks, old speech after a reconnect = 0 | every `_ms` metric in `tests/eval/voice-thresholds.json` (also enforced when `--thresholds` names a file) |
| `tests/bench-e2e.mjs` | functional checks only; it measures, never asserts a time | (none; `--publish` has its own [noise check](#published-artifact)) |
| `tests/early-player.mjs`, `test:lab-reconnect` | ordering (audio before the response completes, Stop aborts it); their waits are timeouts, not limits | (none) |

## Ports

| User | Ports |
| --- | --- |
| Integration engine | `BUDDY_TEST_ENGINE_PORT` (default 16420), +1 api peer, +10 metrics |
| Integration model mock | `BUDDY_TEST_MODEL_PORT` (default 19091) |
| E2E bench, reconnect test, voice eval | `BENCH_PORT_BASE` (default 17500): engine +20 (+21, +30), server +80, model +91, speech +92, fault proxy +93, robot panel +94 (`BUDDY_SCREEN_ADDR`) |
| Robot loopback panel on a device | 8090 unless `BUDDY_SCREEN_ADDR` moves it; `test:shutdown` turns it off |
| Local lab compose | 18080, 17420 |

The integration run and the bench use different ports and can run side by side. Two stacks
(bench, reconnect test, voice eval) can run together with different `BENCH_PORT_BASE`
values; `test:shutdown` uses only ephemeral ports. Measuring an older checkout (`--repo`)
whose robot predates `BUDDY_SCREEN_ADDR` puts its panel back on 8090. Each script refuses to
start (exit 2) when a port is busy; `lsof -iTCP:<port> -sTCP:LISTEN` shows who holds it.

## E2E latency bench

The bench, the reconnect test and the voice eval share one stack, `tests/lib/stack.mjs`,
and one set of turn and barge-in checks, `tests/lib/voice-turns.mjs`.

`bun run bench:e2e` builds release binaries, then runs the real `robot-buddy-server` (it
spawns its own engine on isolated ports), enrolls and owner-claims a device through the
server and the Rivet gateway, and starts the real `robot-buddy-robot --simulate` with
`BUDDY_SPEECH=true`. Utterances go to the robot's stdin; its event lines come back on stdout.

- Fake model: SSE, first token at 250 ms, then 4 characters every 15 ms; replies of 2 to 3
  sentences (12 for barge-in turns).
- Fake speech (`TTS_BASE_URL`): `POST /v1/audio/speech` returns `audio/pcm` after 300 ms, then
  24 kHz mono s16 in real time (48 kB/s), 60 ms of audio per input character. Every byte of a
  response is that request's tag (1 to 255), so the audio a player receives names the
  sentences it came from.
- Fake `aplay` on `PATH` (`tests/lib/fake-aplay.mjs`): timestamps every PCM chunk and its first
  byte (the tag), keeps
  playing its buffer after stdin closes like aplay, and its socket closing marks the kill.
  It is launched once before measuring, because its first launch from the fresh work
  directory sometimes takes 3 s on macOS (see [e2e latency](research/e2e-latency.md)).

Measures: submit to `turn.started`, first `text.delta`, first `speech.ready`, first PCM byte
at the player, `turn.completed`; silence inside a reply (a chunk arriving after the buffered
audio ran out, 20 ms floor); barge-in to player killed, to the turn's end event and to the
model stream closing; server and engine RSS. Percentiles are nearest-rank, so with 20 turns
p95 is the 19th value and p99 is the maximum.

Checks, with speech faked. Each failure is listed in the JSON's `failures` with expected and
observed values, and the bench exits 1:

- every ordinary turn (warm-up included) plays PCM at the player, its player exits cleanly
  (stdin closed, buffered audio played out), and it receives exactly the samples the fake
  provider sent for that turn, from every sentence;
- every barged turn ends in `turn.cancelled`, and its player is stopped (it exits before
  playing out what it was sent);
- every turn completes, and nothing outlives shutdown.

Every run's JSON records `git` (full SHA, `git describe --always --dirty`, dirty flag; untracked
files count as dirty), `machine` (OS, CPU model and count, memory, Bun and rustc
versions), the load average at start and end, and `noise` (`tests/lib/provenance.mjs`).

#### Published artifact

`--publish` writes the raw JSON to `docs/research/e2e-latency.json`; `bun run bench:report`
regenerates the doc's table from it. It refuses (exit 3) before starting when the tree is
dirty or `--repo` names another checkout, and after the run it does not write the file when
a check failed or the noise check failed. The noise check passes when the 1-minute load
average per CPU stays at or under 0.5 at start and end, the fake model and speech servers
sent at most 5 frames 20 ms or more behind their own schedule, and none more than 50 ms
late. The fakes' lateness is host noise by construction: on a quiet machine it is zero.

`bun run bench:report:check` fails when the doc's table differs from the JSON, the JSON
was measured on a dirty tree or did not pass its noise check, or any measured path changed
between the JSON's commit and HEAD: `apps`, `crates`, `tests/lib`, `tests/bench-e2e.mjs`,
`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml`, without Markdown and the Rust test and
bench targets. So publishing, then committing the JSON and the table (a docs-only commit),
keeps it valid, and the next source change makes it stale until it is republished. The
check needs the measured commit in the clone (CI checks out full history).

Options: `--turns N` (20), `--barge N` (5), `--out file.json`, `--no-build`, `--no-speech`,
`--keep` (keep the work directory, including `server.log`; `BENCH_SERVER_LOG` sets its
`RUST_LOG`), `--repo DIR` and `--label NAME` to measure another checkout. A server without `TTS_BASE_URL` is never given `TTS_MODEL`, so it cannot reach the
real provider; only text and `speech.ready` are measured there. The bench refuses to start
if a port is busy and exits non-zero if a turn fails or the server, engine or a listener
outlives shutdown. Credentials are generated per run in a temporary directory and deleted.

## Reconnect test

`bun run test:reconnect` (`tests/robot-reconnect.mjs`) runs the same stack with a fault proxy
(`tests/lib/fault-proxy.mjs`) between the robot and the engine: the server hands the robot the
proxy as its Rivet endpoint, so the robot's actor WebSocket (its conversation actions and pushed
events) and its bot actions (HTTP) pass through it. The proxy unmasks each client WebSocket
frame to find the command it carries.
Each reply sentence carries its turn's marker word, so every request at the fake provider and
every player is attributed to a turn.

1. Turn A plays to the end.
2. Long turn B plays. The proxy holds everything the engine sends on the robot's WebSocket
   (a silent socket), so the robot's 2 s live catch-up replay fires and its response is held
   too. A wake (`{"wake":true}`) arrives. B must end in `turn.cancelled`, and its player must
   stop while the replay is still held (the proxy holds it until the stop, at most 3 s
   after the wake, and then until at least 2 s after the wake and 0.5 s after the stop): the device loop serves input first and never waits on a
   call. The 1000 ms wake-to-stop limit is enforced only under `BUDDY_STRICT_TIMING=1`.
3. Turn C's submit reaches the actor, but the proxy discards the ack and cuts the
   connection. The robot reconnects and resubmits with the same request ID.
4. Turn D plays: the positive control that speech still works after the reconnect.

Then, counted after D: C ran as exactly one actor turn (one turn ID in the robot's events
and one model call), C was never spoken (zero speech requests and zero player samples; its
ack never arrived, and only newly acknowledged turns are spoken), and no speech request for
A or B followed the reconnect. A check may carry a known bug (`xfail`), reported as XFAIL
and failed only under `--strict`; none does today. Options: `--no-build`, `--strict`,
`--out file.json`, `--keep`.

## Voice eval

`bun run eval:voice` (`tests/eval-voice.mjs`) runs ordinary turns, barge-ins and the reconnect
scenario on one stack and writes a JSON scorecard (stdout, or `--out file.json`). The pass
thresholds and the turn and barge-in counts live only in `tests/eval/voice-thresholds.json`
(`--thresholds file` swaps it):

| Metric | Stat |
| --- | --- |
| submit → first `speech.ready` | p95 |
| submit → first PCM at the player | p95 |
| longest silence between sentences in a turn | max |
| barge-in → player stop | p95 |
| barge-in → model stream closed | p95 |
| speech requests for old turns after a reconnect | value, must be 0 |

The five time metrics are wall-clock limits: each score is reported with `enforced`, and an
over-limit one fails the eval only under `BUDDY_STRICT_TIMING=1` or when `--thresholds`
names a file. The old-speech count is always enforced. A functional failure from the bench
checks or the reconnect test fails the eval whatever the scores; a check marked as a known
bug goes to `known_bugs` instead (none today). The scorecard records the same `git`,
`machine` and `noise` as the bench. Exit 0 on pass, 1 on fail, 2 when a port is busy.
Options: `--no-build`, `--out`, `--thresholds`, `--keep`.

## Fixed bugs pinned by tests

Fixed on November 23, 2026; each test failed before its fix.

- `voice_rules.rs::longest_overlapping_match_wins_across_start_positions`: overlapping rules
  resolved leftmost-first; now longest-first as `docs/architecture/voice.md` says.
- `voice_rules.rs::model_authored_tags_do_not_reach_the_provider` and
  `model_markup_is_removed_with_tidy_spacing`: model-authored bracket tags reached the
  Eleven v4 input. A markup-only chunk is now skipped without a provider call
  (`apps/server/src/speech/tests.rs::an_all_markup_turn_has_no_speech`,
  `markup_chunks_are_skipped_and_the_reply_continues`,
  `openrouter_skips_markup_through_the_pipeline`).
- `protocol_golden.rs::settings_read_back_ignore_additive_fields` and
  `settings_writes_reject_unknown_fields` replace the old test that pinned strict settings
  types, which contradicted "Clients ignore additive unknown fields".
- `tests/robot-shutdown.mjs`, `input.rs::an_open_input_does_not_delay_runtime_shutdown` and
  `audio.rs::runtime_shutdown_kills_an_abandoned_player`: the robot ignored SIGTERM while
  stdin was open, because dropping the Tokio runtime waited on the blocking
  `tokio::io::stdin` read. Stdin is now read on a plain OS thread, runtime shutdown is
  bounded at 100 ms, and the cache is flushed and the player killed before exit.

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
