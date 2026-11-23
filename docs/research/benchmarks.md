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

`tests/history_sql.rs` checks the plans. The recent-turns read searches `turns_completed`. The trigger's delete plans as `VIRTUAL TABLE INDEX 0:=` (rowid lookup), where v1 planned `INDEX 0:` (full scan).

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
