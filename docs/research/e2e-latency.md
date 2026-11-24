# End-to-end latency

Measured on November 23, 2026 with `bun run bench:e2e` ([how it works](../testing.md#e2e-latency-bench)).
Real server binary, real Rivet engine 3.1.1 and real robot client in `--simulate` mode;
the model, the speech provider and `aplay` are fakes with fixed timing:

- model: first token at 250 ms, then 4 characters every 15 ms, 2 to 3 sentence replies
- speech: 300 ms to first byte, then real-time 24 kHz mono PCM (48 kB/s)

So these numbers are our overhead on top of fixed provider latency, not what a user hears
with a real provider. The floor for the first sentence is about 360 ms (250 ms first token
plus about 110 ms to stream the first sentence); first audio adds the 300 ms speech
time to first byte.

## Setup

- Mac mini, Apple M4 Pro (14 cores, macOS arm64), release builds, shared with other build jobs.
  Load average was 22 to 30 during the runs below. Absolute values will be lower on an
  idle machine; compare the two columns, not the numbers against a target.
- 50 measured turns and 10 barge-in trials per run, after one warm-up turn.
- Percentiles are nearest-rank. With 50 turns p99 is the maximum; with 10 barge-ins
  p95 and p99 are the maximum.
- Baseline is 983e90f. Current is wave2-robot at 120c195 (no source changes in this
  worktree). The two runs were back to back.

Baseline has no `TTS_BASE_URL`; its server hardcodes the OpenRouter host. The speech path
cannot be faked there without changing its source, so the bench gives it no `TTS_MODEL`
and measures only text and `speech.ready`. First audio, silence gaps and player kill time
exist for the current tree only.

## Results (ms)

| Measure | Baseline p50 | p95 | p99 | Current p50 | p95 | p99 | Current max |
| --- | --- | --- | --- | --- | --- | --- | --- |
| submit → `turn.started` | 68 | 172 | 298 | 69 | 122 | 261 | 261 |
| submit → first `text.delta` | 410 | 545 | 635 | 426 | 550 | 594 | 594 |
| submit → first `speech.ready` | 456 | 588 | 698 | 465 | 568 | 652 | 652 |
| submit → first PCM byte at the player | n/a | n/a | n/a | 833 | 964 | 1016 | 1016 |
| submit → `turn.completed` | 664 | 781 | 869 | 668 | 779 | 814 | 814 |
| silence inside a reply (≥ 20 ms), per gap | n/a | n/a | n/a | 34 | 93 | 93 | 93 |
| barge-in → player killed | n/a | n/a | n/a | 4.5 | 56 | 56 | 56 |
| barge-in → turn end event (`turn.cancelled`) | 69 | 150 | 150 | 88 | 164 | 164 | 164 |
| barge-in → model stream closed | 52 | 139 | 139 | 56 | 127 | 127 | 127 |

The JSON from `--out` is not checked in; rerun the bench to regenerate it.

Other current runs the same evening (p50 / p95 of first PCM byte): 842 / 1024 and
781 / 920 (50 turns), 776 / 1039 and 850 / 981 (20 turns). Run to run spread at this load
is about ±60 ms at p50, larger than the baseline-to-current difference in every text
measure.

## Reading the numbers

- Text path: no measurable change. Baseline and current are within run-to-run noise at
  p50, p95 and p99 for `turn.started`, first `text.delta`, first `speech.ready` and
  `turn.completed`. About 60 ms of each p50 is submit to `turn.started` (robot to server to
  actor and back); first `speech.ready` sits about 100 ms above the 360 ms floor.
- First audio: `speech.ready` + about 370 ms, of which 300 ms is the fake provider. The
  robot requests audio at submit and the server holds the response until the first chunk
  is ready, so no robot round trip is added after `speech.ready`.
- Sentence boundaries: with lookahead the next sentence is already synthesized when the
  previous one ends. In 50 turns (2.5 speech requests per turn), 8 turns had one gap of 25 to
  93 ms. In the same run the fake speech server itself fell behind real time 283 times
  (p50 31 ms, max 167 ms) because the bench process shares the loaded machine, so these gaps
  cannot be attributed to the server. Two other runs had no gap or one 25 ms gap. One early
  run, before the bench recorded fake lateness, had a single 1773 ms gap at load average
  38; it did not recur.
- Barge-in: the player is killed within 5 ms at p50 (SIGKILL, the ALSA buffer is not
  drained). The cancel and the closed provider stream follow in tens of milliseconds. Both
  trees are similar; the difference is within noise.
- Memory: server RSS stays at about 40 MB in both trees (42 MB current) and does not grow
  over 70 turns. Engine RSS grows from about 70 to 110-125 MB over the run in both trees
  (baseline 70 → 126 MB, current 74 → 112 MB). Whether that levels off needs a longer run.
- One model call per turn in both trees.

## Observed, not explained

- Warm-up turn first audio was 4.3 s and 4.8 s in two of five current runs (1.1 to 1.4 s in
  the others) while its `speech.ready` was normal. Only the first speech request of a fresh
  server is affected. Worth a trace of the first `/v1/speech` on a cold server.

## Found while benching

- The current robot does not exit on SIGTERM while its stdin is open (baseline exits in
  about 50 ms). See `tests/robot-shutdown.mjs` and the known bugs in [testing](../testing.md).
