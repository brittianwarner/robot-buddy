// End-to-end latency bench: real server binary + its engine + the real robot client in
// --simulate mode, against a fake streaming model and a fake speech provider
// (tests/lib/stack.mjs has the fakes and their timing).
//
//   bun run bench:e2e                       # build release, 20 turns, 5 barge-ins
//   bun tests/bench-e2e.mjs --turns 50 --barge 10 --out /tmp/bench.json
//   bun tests/bench-e2e.mjs --publish       # also write docs/research/e2e-latency.json
//   bun tests/bench-e2e.mjs --repo ../rb-baseline --label baseline
//
// It fails (exit 1) when, with speech faked:
//   - an ordinary turn plays no PCM, ends without a clean player exit, or plays a different
//     number of samples than the fake provider sent for it;
//   - a barged turn does not end in turn.cancelled, or its player is not stopped;
// and always when a turn does not complete or anything outlives shutdown. Each failure is
// listed in `failures` with the expected and observed values.
//
// Opt-in and local only. It refuses to start if any of its ports is in use (exit 2).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { gitDescribe, rss, sleep, startStack, stats } from './lib/stack.mjs';
import { voiceTurns } from './lib/voice-turns.mjs';

const argv = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : fallback;
};
const has = (name) => argv.includes(`--${name}`);
const root = path.resolve(import.meta.dir, '..');
const repo = path.resolve(opt('repo', root));
const label = opt('label', path.basename(repo));
const TURNS = Number(opt('turns', process.env.BENCH_TURNS ?? 20));
const BARGES = Number(opt('barge', process.env.BENCH_BARGE ?? 5));
const out = opt('out', has('publish') ? path.join(root, 'docs/research/e2e-latency.json') : null);
const log = (...a) => console.error('bench:', ...a);

let stack;
try {
  stack = await startStack({ repo, build: !has('no-build'), speech: !has('no-speech'), keep: has('keep'), log });
} catch (e) {
  log(e.code === 'PORTS_BUSY' ? `refusing to start, ${e.message}` : `FAILED to start: ${e.message}`);
  if (e.logs) log(e.logs);
  process.exit(e.code === 'PORTS_BUSY' ? 2 : 1);
}
const { SPEECH, speech, model } = stack;
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, async () => {
    log(`${sig}: shutting down`);
    await stack.shutdown();
    process.exit(130);
  });
}

const result = {
  label,
  commit: gitDescribe(repo),
  date: new Date().toISOString(),
  platform: `${os.platform()} ${os.arch()} ${os.cpus()[0]?.model ?? ''}`.trim(),
  load_average: os.loadavg().map((l) => Math.round(l * 10) / 10),
  rivetkit: stack.rivetVersion,
  turns: TURNS,
  barge_ins: BARGES,
  speech_faked: SPEECH,
  note: SPEECH ? null : 'speech path not faked (the server has no TTS_BASE_URL); only text and speech.ready latency measured',
};
const failures = [];
const fail = (what, detail) => {
  failures.push({ what, ...detail });
  log(`FAIL ${what}: ${JSON.stringify(detail)}`);
};
const { turn, barge } = voiceTurns(stack, { fail, log });

let exit = 0;
try {
  const rssSamples = [];
  const sampler = setInterval(() => rssSamples.push([rss(stack.serverPid), rss(stack.enginePid)]), 250);

  const cold = await turn('Hello there, this is the warm-up turn.', { name: 'warm-up' });
  const ms = (v) => v?.toFixed(0) ?? '-';
  log(
    `warm-up: ${cold.kind}, speech.ready ${ms(cold.ready)} ms, provider request ${ms(cold.speech_request)} ms, ` +
      `fake aplay started ${ms(cold.player_start)} ms, first pcm ${ms(cold.pcm)} ms`,
  );
  const rssStart = [rss(stack.serverPid), rss(stack.enginePid)];
  const modelCallsBefore = model.calls;
  const speechBefore = speech.requests.length;
  const turns = [];
  for (let i = 0; i < TURNS; i++) {
    const r = await turn(`Bench turn ${i + 1}. Tell me something short.`, { name: `turn ${i + 1}` });
    turns.push(r);
    log(`turn ${i + 1}/${TURNS}: ${r.kind} ready ${ms(r.ready)} pcm ${ms(r.pcm)} gaps [${r.gaps.map((g) => g.toFixed(0))}]`);
    await sleep(200);
  }

  const barges = [];
  for (let i = 0; i < BARGES; i++) barges.push(await barge(i + 1, { name: `barge ${i + 1}/${BARGES}` }));
  clearInterval(sampler);

  const allGaps = turns.flatMap((r) => r.gaps);
  const server = rssSamples.map((s) => s[0]).filter(Boolean);
  const engine = rssSamples.map((s) => s[1]).filter(Boolean);
  Object.assign(result, {
    failed_turns: [cold, ...turns].filter((r) => r.kind !== 'turn.completed').length,
    cold_turn_ms: {
      speech_ready: cold.ready,
      speech_request: cold.speech_request,
      player_start: cold.player_start,
      first_pcm: cold.pcm,
      completed: cold.done,
    },
    metrics_ms: {
      submit_to_turn_started: stats(turns.map((r) => r.started)),
      submit_to_first_text_delta: stats(turns.map((r) => r.text)),
      submit_to_first_speech_ready: stats(turns.map((r) => r.ready)),
      submit_to_first_pcm_byte: stats(turns.map((r) => r.pcm)),
      submit_to_turn_completed: stats(turns.map((r) => r.done)),
      silence_gap_within_reply: {
        ...stats(allGaps),
        floor_ms: 20,
        turns_with_gap: turns.filter((r) => r.gaps.length).length,
        total_ms: Math.round(allGaps.reduce((a, b) => a + b, 0)),
      },
      max_silence_per_turn: stats(turns.map((r) => (r.audio ? Math.max(0, ...r.gaps) : null))),
      barge_in_to_audio_closed: stats(barges.map((b) => b.to_audio_closed)),
      barge_in_to_turn_terminal_event: stats(barges.map((b) => b.to_terminal_event)),
      barge_in_to_provider_stream_closed: stats(barges.map((b) => b.to_provider_closed)),
    },
    audio: SPEECH
      ? {
          turns_checked: turns.length + 1,
          expected_samples: [cold, ...turns].reduce((a, r) => a + r.audio.expected_samples, 0),
          observed_samples: [cold, ...turns].reduce((a, r) => a + r.audio.observed_samples, 0),
          barges_stopped: barges.filter((b) => b.player_stopped).length,
        }
      : null,
    rss_kb: {
      server: { after_warmup: rssStart[0], max: Math.max(...server), end: server.at(-1) },
      engine: { after_warmup: rssStart[1], max: Math.max(...engine), end: engine.at(-1) },
    },
    fake_speech_fell_behind_ms: stats(speech.late.map((l) => l.ms)),
    per_turn: turns.map((r) => ({
      kind: r.kind,
      ready: r.ready && Math.round(r.ready),
      pcm: r.pcm && Math.round(r.pcm),
      done: r.done && Math.round(r.done),
      gaps: r.gaps.map(Math.round),
      samples: r.audio && [r.audio.expected_samples, r.audio.observed_samples],
    })),
    per_barge: barges.map((b) => ({
      end_kind: b.end_kind,
      audio_closed: b.to_audio_closed && Math.round(b.to_audio_closed * 10) / 10,
      terminal_event: Math.round(b.to_terminal_event),
      provider_closed: b.to_provider_closed && Math.round(b.to_provider_closed),
      player_stopped: b.player_stopped,
    })),
    model_calls_per_turn: (model.calls - modelCallsBefore) / (TURNS + 2 * BARGES),
    speech_requests_per_turn: SPEECH ? (speech.requests.length - speechBefore) / (TURNS + 2 * BARGES) : null,
    barge_end_kinds: [...new Set(barges.map((b) => b.end_kind))],
  });
} catch (e) {
  fail('bench aborted', { error: e.message });
  log(stack.logs());
} finally {
  await stack.shutdown();
}
if (stack.leaks.length) log(`LEAK:\n  ${stack.leaks.join('\n  ')}`);
result.failures = failures;
result.leaks = stack.leaks;
result.findings = stack.findings;
if (failures.length || stack.leaks.length) exit = 1;

const json = JSON.stringify(result, null, 2);
if (out) fs.writeFileSync(out, `${json}\n`);
console.log(json);
const m = result.metrics_ms;
if (m) {
  const row = (name, s) => `| ${name} | ${s.n ?? 0} | ${s.p50 ?? '-'} | ${s.p95 ?? '-'} | ${s.p99 ?? '-'} | ${s.max ?? '-'} |`;
  console.error(
    [
      `\n${label} (${result.commit}), ms`,
      '| Measure | n | p50 | p95 | p99 | max |',
      '| --- | --- | --- | --- | --- | --- |',
      row('submit → turn.started', m.submit_to_turn_started),
      row('submit → first text.delta', m.submit_to_first_text_delta),
      row('submit → first speech.ready', m.submit_to_first_speech_ready),
      row('submit → first PCM byte at player', m.submit_to_first_pcm_byte),
      row('submit → turn.completed', m.submit_to_turn_completed),
      row('silence gap inside a reply (≥ 20 ms)', m.silence_gap_within_reply),
      row('barge-in → player killed', m.barge_in_to_audio_closed),
      row('barge-in → turn end event', m.barge_in_to_turn_terminal_event),
      row('barge-in → model stream closed', m.barge_in_to_provider_stream_closed),
      row('(fake speech server fell behind, ≥ 20 ms)', result.fake_speech_fell_behind_ms),
    ].join('\n'),
  );
}
log(failures.length ? `${failures.length} failure(s)` : 'all checks passed');
process.exit(exit);
