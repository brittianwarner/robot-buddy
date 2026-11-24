# Backend benchmarks

Measured 2026-11-23 on branch `perf/core`.

## Machine

- Apple M4 Pro, 14 cores, 64 GB, macOS 26.6.2
- rustc 1.97.1, `cargo bench` release profile
- Load average was 39 to 52 during every run. Other builds shared the machine.

Read the numbers with that load in mind. Identical code varied by up to 2x between runs. Compare rows from the same run, not across runs. Criterion reports `[low estimate high]`.

## Commands

```sh
cargo bench -p robot-buddy --bench sentences -- --warm-up-time 1 --measurement-time 3
cargo bench -p robot-buddy --bench primitives -- --warm-up-time 1 --measurement-time 3
cargo bench -p robot-buddy --bench history_sql -- --warm-up-time 1 --measurement-time 3
bun run test:integration
```

## Sentence splitter

`legacy` is a frozen copy of the splitter before the rewrite (`benches/legacy_sentences.rs`). Every run measures both, so `legacy` is the baseline for its own run. The reply is 393 bytes. `long_unpunctuated` is 4,400 bytes in 4-byte pieces.

| Case | Before: legacy, same run | After run 1: current / legacy | After run 2: current / legacy |
|---|---|---|---|
| whole_reply | 6.68 µs | 4.76 µs / 11.57 µs | 6.33 µs / 4.90 µs |
| token_fragments (4 bytes) | 48.9 µs | 10.6 µs / 80.9 µs | 10.7 µs / 36.1 µs |
| char_fragments (1 char) | 117 µs | 14.4 µs / 78.2 µs | 6.28 µs / 123 µs |
| long_unpunctuated | 3.27 ms | 88.1 µs / 2.12 ms | 51.0 µs / 1.19 ms |

- Fragmented input is 3x to 20x faster than legacy in both runs. The old splitter rescanned its whole buffer on every push. The new one keeps a cursor.
- Long unpunctuated text is 23x to 24x faster in both runs.
- A whole reply in one push shows no consistent difference. Run 1 favours current and run 2 favours legacy.

## Primitives

These were not changed. One run records the current cost.

| Bench | Time |
|---|---|
| auth/sign_session | [1.96 2.56 3.34] µs |
| auth/verify_session | [1.79 2.11 2.48] µs |
| voice/speech_body_32_rules | [5.75 6.40 7.11] µs |
| audio/pcm_format_parse | [71.1 98.5 141.9] ns |

## History SQL at 10,000 turns

Local in-memory SQLite (rusqlite, bundled), with the real schema files. `v1` is `tests/fixtures/conversation-v1.sql`. The production engine runs SQLite remotely, so a round trip adds to both columns.

| Query | v1 | current |
|---|---|---|
| Latest 12 completed turns | [6.79 7.67 8.66] ms | [10.8 13.4 16.3] µs |
| Finish a turn (answer trigger) | [1.74 2.05 2.42] ms | [37.0 44.9 54.6] µs |

`tests/history_sql.rs` checks the plans. The recent-turns read searches `turns_completed`. The trigger's keyed row access plans as `VIRTUAL TABLE INDEX 0:=` (rowid lookup), where v1 planned `INDEX 0:` (full scan).

## Rebuild batches at 5,000 turns

`cargo test -p robot-buddy --test history_sql -- --nocapture five_thousand` prints this. It rebuilds a version 1 database with 5,000 turns on a WAL file with automatic checkpoints off. Pages are the WAL frames one commit wrote, after a truncating checkpoint.

| Measure | Value |
|---|---|
| Batches (index, then sweep) | 42 |
| Largest batch | 84 pages |
| Start transaction | 59 pages |
| One-shot rebuild it replaced | 260 pages in one transaction |
| Rivet limit per transaction | 32,768 pages |

Batch size depends on the batch, not on history. The one-shot rebuild grows with every turn stored.

## SQL per turn

Measured 2026-11-23 by `tests/core_sql_counts.rs`, which reads the counters of the `db::Db` wrapper every actor statement goes through. Counts are for the whole test process: the Bot, Conversation and Memory actors, plus the test client's own reads. A transaction's statements count as statements; an auto-commit statement is its own commit. Before is the code before the change, with only the counting wrapper added.

```sh
cargo test -p robot-buddy --test core_sql_counts -- --ignored --nocapture
```

Run with the environment of `bun run test:integration`.

| Measure | Statements before | after | Auto-commit before | after | Transactions before | after |
|---|---|---|---|---|---|---|
| Direct spoken turn (2 sentences) | 37 | 31 | 23 | 11 | 5 | 6 |
| One tool turn (`memory_remember`, then a spoken answer) | 63 | 53 to 58 | 51 | 35 to 40 | 4 | 5 |
| Enroll plus first turn (new database) | 76 | 71 to 79 | 54 | 42 to 50 | 9 | 11 |
| Wake after an engine restart plus a direct turn | not measured | 42 to 49 | not measured | 24 to 31 | not measured | 5 |

Before is one run. After is four runs for the first three rows and three for the wake row; the direct turn gave the same counts each time. The spread is auto-commit reads whose number depends on timing: live access checks (one Bot statement each, at most once a second during a turn) and the test client's replay reads.

What changed on the turn path:

- The claim, `turn.started` and every read the first model round needs commit in one `conversation.claim` transaction. A fresh run skips the completed-tool and spoken-byte reads, and later rounds keep both in memory.
- The duplicate `ready` checkpoint before a tool round is gone, and settings tools no longer write their receipt twice.
- Text between sentence ends commits at most every 200 ms instead of on a 100 ms tick, and the flush clock restarts at each commit.
- The drain no longer rechecks for a queued turn after each turn; a submit during a drain asks it to claim again.

The transaction count rises by one because the claim is now a transaction instead of auto-commit statements. A wake no longer reruns the schema, the seeds and factory adoption once `PRAGMA user_version` is set; a new database still runs them.

## Turn latency (integration)

`tests/actors.rs` runs against a real Rivet engine with a mock provider. The mock streams 4-character SSE tokens every 20 ms. Times are measured by the test client, which polls replay every 5 ms.

| Measure | Run 1 | Run 2 |
|---|---|---|
| Submit to first `speech.ready` | 228 ms | 197 ms |
| Gaps between `speech.ready` (4 chunks) | 241, 331, 146 ms | 185, 320, 148 ms |
| Mock stream duration | 680 ms | 680 ms |
| Submit to last `speech.ready` | 948 ms | 852 ms |
| Barge-in: cancel sent to provider connection closed | 183 ms | 10 ms |
| Barge-in: cancel action round trip | 184 ms | 11 ms |
| Events committed after `turn.cancelled` | 0 | 0 |

The latency eval is new, so there is no before number. The close time includes the cancel action round trip. The connection closes as soon as the cancellation commits.

## Robot dependency tree

Measured 2026-11-23 on branch `wave2-core`. The robot now links `robot-buddy` with `default-features = false` and talks to actors through the untyped `rivetkit-client`, so it no longer links `rivetkit-core`, Rig or `chrono-tz`.

```sh
cargo tree -p robot-buddy-robot -e normal | wc -l
cargo tree -p robot-buddy-robot -e normal --prefix none --no-dedupe | sort -u | wc -l
cargo build --release -p robot-buddy-robot && ls -l target/release/robot-buddy-robot
```

| Measure | Before | After | Change |
|---|---|---|---|
| `cargo tree` lines (normal deps) | 976 | 508 | −48% |
| Unique crates | 272 | 184 | −32% |
| Release binary (macOS aarch64, not stripped) | 8,706,816 B | 8,453,808 B | −253,008 B (−2.9%) |

- The binary shrinks much less than the tree. The linker already dropped most unused server code. The tree change cuts build time and the supply-chain surface the Pi carries.
- The baseline release build took 9 m 54 s from cold. The after build was incremental, so build times are not compared.
- tokio stays on `full`: `rivetkit-client` enables it, so trimming the robot's own features changes nothing.
