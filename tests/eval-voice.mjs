// Voice-turn eval: one scored run of the local stack, written as a JSON scorecard.
//
//   bun run eval:voice                           # build release, scorecard to stdout
//   bun tests/eval-voice.mjs --no-build --out /tmp/voice-scorecard.json
//   bun tests/eval-voice.mjs --thresholds my-thresholds.json
//
// Runs ordinary spoken turns, barge-ins and the reconnect scenario of
// tests/robot-reconnect.mjs on one stack (tests/lib/stack.mjs), then scores six metrics
// against tests/eval/voice-thresholds.json, the only place thresholds live:
//   submit → first speech.ready, submit → first PCM at the player, max silence between
//   sentences, barge-in → player stop, barge-in → model stream closed, and speech
//   requests for old turns after a reconnect (must be 0).
// Functional failures (no audio, wrong sample count, a barge that is not cancelled, a
// failed reconnect check) fail the eval whatever the scores. A check marked as a known bug
// is reported in `known_bugs` and does not fail it.
//
// Exit 0 when every metric passes and no functional check failed, 1 otherwise, 2 when a
// port is busy. Same port rules as the bench (BENCH_PORT_BASE).
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { gitDescribe, sleep, startStack, stats } from './lib/stack.mjs';
import { voiceTurns } from './lib/voice-turns.mjs';
import { reconnectReply, reconnectScenario } from './robot-reconnect.mjs';

const argv = process.argv.slice(2);
const has = (name) => argv.includes(`--${name}`);
const opt = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : fallback;
};
const root = path.resolve(import.meta.dir, '..');
const config = JSON.parse(fs.readFileSync(opt('thresholds', path.join(root, 'tests/eval/voice-thresholds.json')), 'utf8'));
const log = (...a) => console.error('eval:', ...a);

let stack;
try {
  stack = await startStack({ build: !has('no-build'), proxy: true, reply: reconnectReply, keep: has('keep'), log });
} catch (e) {
  log(e.code === 'PORTS_BUSY' ? `refusing to start, ${e.message}` : `FAILED to start: ${e.message}`);
  if (e.logs) log(e.logs);
  process.exit(e.code === 'PORTS_BUSY' ? 2 : 1);
}

const failures = [];
const knownBugs = [];
const fail = (what, detail) => {
  failures.push({ what, ...detail });
  log(`FAIL ${what}: ${JSON.stringify(detail)}`);
};
const observed = {};
try {
  if (!stack.SPEECH) throw new Error('this tree cannot fake the speech provider (no TTS_BASE_URL)');
  const { turn, barge } = voiceTurns(stack, { fail, log });
  await turn('Hello there, this is the warm-up turn.', { name: 'warm-up' });
  const turns = [];
  for (let i = 0; i < config.turns; i++) {
    turns.push(await turn(`Eval turn ${i + 1}. Tell me something short.`, { name: `turn ${i + 1}` }));
    await sleep(200);
  }
  const barges = [];
  for (let i = 0; i < config.barges; i++) barges.push(await barge(i + 1, { name: `barge ${i + 1}` }));
  const reconnect = await reconnectScenario(stack, { log });
  for (const c of reconnect.checks) {
    if (c.ok) continue;
    if (c.xfail) knownBugs.push({ check: c.name, bug: c.xfail, ...c.detail });
    else fail(c.name, c.detail);
  }
  observed.submit_to_first_speech_ready_ms = stats(turns.map((r) => r.ready));
  observed.submit_to_first_pcm_at_player_ms = stats(turns.map((r) => r.pcm));
  observed.max_inter_sentence_silence_ms = stats(turns.map((r) => Math.max(0, ...r.gaps)));
  observed.barge_in_to_player_stop_ms = stats(barges.map((b) => b.to_audio_closed));
  observed.barge_in_to_model_stream_closed_ms = stats(barges.map((b) => b.to_provider_closed));
  observed.old_speech_after_reconnect = { value: reconnect.metrics.old_speech_requests_after_reconnect };
  observed.reconnect = reconnect.metrics;
  observed.fake_speech_fell_behind_ms = stats(stack.speech.late.map((l) => l.ms));
} catch (e) {
  fail('eval aborted', { error: e.message });
  log(stack.logs());
} finally {
  await stack.shutdown();
}
for (const leak of stack.leaks) fail('nothing outlives shutdown', { leak });

const scores = Object.entries(config.metrics).map(([name, { stat, max }]) => {
  const value = observed[name]?.[stat] ?? null;
  return { metric: name, stat, value, max, n: observed[name]?.n ?? null, pass: value !== null && value <= max };
});
const pass = failures.length === 0 && scores.every((s) => s.pass);
const scorecard = {
  pass,
  commit: gitDescribe(stack.repo),
  date: new Date().toISOString(),
  platform: `${os.platform()} ${os.arch()} ${os.cpus()[0]?.model ?? ''}`.trim(),
  load_average: os.loadavg().map((l) => Math.round(l * 10) / 10),
  thresholds: path.relative(root, opt('thresholds', path.join(root, 'tests/eval/voice-thresholds.json'))),
  turns: config.turns,
  barges: config.barges,
  scores,
  failures,
  known_bugs: knownBugs,
  observed,
  leaks: stack.leaks,
};
const json = JSON.stringify(scorecard, null, 2);
if (opt('out', null)) fs.writeFileSync(opt('out', null), `${json}\n`);
console.log(json);
for (const s of scores) log(`${s.pass ? 'pass' : 'FAIL'} ${s.metric} ${s.stat}=${s.value ?? '-'} (max ${s.max})`);
log(pass ? 'PASS' : `FAIL (${failures.length} functional failure(s), ${scores.filter((s) => !s.pass).length} metric(s) over)`);
process.exit(pass ? 0 : 1);
