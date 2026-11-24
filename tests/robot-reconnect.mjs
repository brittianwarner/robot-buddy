// Simulated device through the real robot binary: interruption during a held replay, a
// reconnect after a lost submit ack, and no old speech afterwards.
//
//   bun run test:reconnect               # build release, run, exit 1 on a failed check
//   bun tests/robot-reconnect.mjs --no-build --strict --out /tmp/reconnect.json
//
// The robot runs with --simulate, the fake aplay and JSON control on stdin; a fault proxy
// (tests/lib/fault-proxy.mjs) sits between it and the engine. Every reply sentence carries
// its turn's marker word, and every PCM byte its speech request's tag, so each request at
// the fake provider and each player is attributed to a turn.
//
// Steps:
//   1. Turn A plays to the end.
//   2. Long turn B plays. Everything the engine sends on the robot's WebSocket is held (a
//      silent socket), so the robot's 2 s live catch-up replay fires and its response is
//      held too. A wake arrives. Checks: B ends turn.cancelled, and its player stops within
//      WAKE_STOP_MS while the replay is still held.
//   3. Turn C's submit runs on the actor but its ack is lost: the connection is cut. The
//      robot reconnects.
//   4. Turn D plays.
//   Then: no speech request for A or B after the reconnect. C's are reported, not checked.
//
// Same port rules as the bench (BENCH_PORT_BASE); refuses busy ports. A check may carry an
// `xfail` (a known bug): reported as XFAIL, and failed only under --strict.
import fs from 'node:fs';
import { gitDescribe, now, sleep, startStack, TERMINAL } from './lib/stack.mjs';

/** Longest acceptable wake → player stop while a replay is outstanding. */
export const WAKE_STOP_MS = 1000;

const MARKERS = { a: 'amber', b: 'birch', c: 'cedar', d: 'dune' };
export const reconnectReply = (text) => {
  const marker = Object.values(MARKERS).find((m) => text.includes(`check ${m}`));
  if (!marker) return null;
  // B streams for about 15 s, so it is still running on the actor when the wake lands.
  const n = marker === MARKERS.b ? 60 : 2;
  return Array.from({ length: n }, (_, i) => `The ${marker} answer has sentence ${i + 1} for this check.`).join(' ');
};

/**
 * Runs the scenario on a stack started with `{ proxy: true, reply: reconnectReply }`.
 * Returns `{ checks, metrics }`; a check is `{ name, ok, xfail?, detail }`.
 */
export async function reconnectScenario(stack, { log = () => {} } = {}) {
  const { proxy, waitEvent, until, say, control, playersFor, requestsFor } = stack;
  const checks = [];
  const metrics = {};
  const check = (name, ok, detail = {}, xfail = null) => {
    checks.push({ name, ok, ...(xfail && !ok ? { xfail } : {}), detail });
    log(`${ok ? 'ok  ' : xfail ? 'XFAIL' : 'FAIL'} ${name} ${JSON.stringify(detail)}`);
  };
  const turnFor = async (marker, t0, ms = 30000) =>
    waitEvent((e) => e.kind === 'turn.started' && e.payload?.text?.includes(`check ${marker}`) && e.t >= t0, ms, `${marker} turn.started`);
  const endOf = (id, ms = 60000) => waitEvent((e) => e.turn_id === id && TERMINAL.has(e.kind), ms, 'turn end');
  const playing = (marker) => until(`${marker} audio`, async () => playersFor(marker).find((p) => p.chunks.length), 30000);

  // 1. A plays to the end.
  {
    const t0 = say(`Reconnect check ${MARKERS.a}.`);
    const started = await turnFor(MARKERS.a, t0);
    const end = await endOf(started.turn_id);
    const player = await playing(MARKERS.a).catch(() => null);
    if (player) await until('amber player exit', async () => player.closed, 30000).catch(() => {});
    check('turn A plays to the end', end.kind === 'turn.completed' && Boolean(player?.end), {
      end: end.kind,
      played: Boolean(player),
      clean_end: Boolean(player?.end),
    });
  }

  // 2. B is interrupted while a replay is held.
  {
    const t0 = say(`Reconnect check ${MARKERS.b}, a long one.`);
    const started = await turnFor(MARKERS.b, t0);
    const player = await playing(MARKERS.b);
    await sleep(300);
    const th = now();
    proxy.holdPushes();
    const held = await Promise.race([proxy.holdNext('replay'), sleep(10000).then(() => null)]);
    if (!held) throw new Error('the robot did not replay within 10 s of its pushes stopping');
    metrics.pushes_held_to_catch_up_replay_ms = Math.round(held - th);
    await sleep(200);
    const streaming = stack.model.open.size > 0;
    const tw = control({ wake: true });
    const stoppedWhileHeld = await until('player stop while the replay is held', async () => player.closed, WAKE_STOP_MS).catch(() => null);
    // Hold a little longer, then let everything through.
    await sleep(2000);
    proxy.release();
    const end = await endOf(started.turn_id, 30000).catch(() => null);
    await until('birch player stop', async () => player.closed, 20000).catch(() => {});
    metrics.wake_to_player_stop_ms = player.closed ? Math.round(player.closed - tw) : null;
    metrics.wake_to_turn_cancelled_ms = end ? Math.round(end.t - tw) : null;
    if (!streaming) throw new Error('turn B finished streaming before the wake; the scenario needs a longer reply');
    check('wake during a held replay: turn B ends turn.cancelled', end?.kind === 'turn.cancelled', { end: end?.kind ?? null });
    check('wake during a held replay: B player stops', Boolean(player.closed && !player.end), {
      wake_to_player_stop_ms: metrics.wake_to_player_stop_ms,
    });
    check(
      `wake during a held replay: B player stops within ${WAKE_STOP_MS} ms`,
      Boolean(stoppedWhileHeld),
      { wake_to_player_stop_ms: metrics.wake_to_player_stop_ms, replay_held_ms: Math.round(now() - held) },
    );
    await sleep(500);
  }

  // 3. C's submit ack is lost; the robot reconnects.
  let reconnected;
  {
    const sockets = () => proxy.seen.filter((s) => s.event === 'websocket').length;
    const before = sockets();
    const dropped = proxy.dropNext('submit');
    const t0 = say(`Reconnect check ${MARKERS.c}.`);
    const cut = await Promise.race([dropped, sleep(15000).then(() => null)]);
    if (!cut) throw new Error('no submit reached the proxy within 15 s');
    await until('robot reconnect', async () => sockets() > before, 60000);
    reconnected = proxy.seen.filter((s) => s.event === 'websocket').at(-1).t;
    metrics.submit_cut_to_reconnect_ms = Math.round(reconnected - cut);
    const started = await turnFor(MARKERS.c, t0, 30000).catch(() => null);
    const end = started ? await endOf(started.turn_id).catch(() => null) : null;
    metrics.cedar_turn_end = end?.kind ?? null;
    check('lost submit ack: the turn ran once on the actor', Boolean(started), { end: end?.kind ?? null });
    await sleep(1000);
  }

  // 4. D plays after the reconnect.
  {
    const t0 = say(`Reconnect check ${MARKERS.d}.`);
    const started = await turnFor(MARKERS.d, t0);
    const end = await endOf(started.turn_id);
    const player = await playing(MARKERS.d).catch(() => null);
    if (player) await until('dune player exit', async () => player.closed, 30000).catch(() => {});
    check('after the reconnect: turn D plays', end.kind === 'turn.completed' && Boolean(player?.end), {
      end: end.kind,
      played: Boolean(player),
      clean_end: Boolean(player?.end),
    });
  }

  const old = [MARKERS.a, MARKERS.b].flatMap((m) => requestsFor(m, reconnected));
  metrics.old_speech_requests_after_reconnect = old.length;
  metrics.cedar_speech_requests = requestsFor(MARKERS.c).length;
  check('after the reconnect: no speech request for an old turn', old.length === 0, {
    observed: old.length,
    expected: 0,
    inputs: old.map((q) => q.input),
  });
  return { checks, metrics };
}

if (import.meta.main) {
  const argv = process.argv.slice(2);
  const has = (name) => argv.includes(`--${name}`);
  const opt = (name) => {
    const i = argv.indexOf(`--${name}`);
    return i >= 0 ? argv[i + 1] : null;
  };
  const log = (...a) => console.error('reconnect:', ...a);
  let stack;
  try {
    stack = await startStack({ build: !has('no-build'), proxy: true, reply: reconnectReply, keep: has('keep'), log });
  } catch (e) {
    log(e.code === 'PORTS_BUSY' ? `refusing to start, ${e.message}` : `FAILED to start: ${e.message}`);
    if (e.logs) log(e.logs);
    process.exit(e.code === 'PORTS_BUSY' ? 2 : 1);
  }
  if (!stack.SPEECH) {
    log('this tree cannot fake the speech provider; nothing to check');
    await stack.shutdown();
    process.exit(1);
  }
  let outcome = { checks: [], metrics: {} };
  try {
    outcome = await reconnectScenario(stack, { log });
  } catch (e) {
    outcome.checks.push({ name: 'scenario ran', ok: false, detail: { error: e.message } });
    log(`FAILED: ${e.message}`);
    log(stack.logs());
  } finally {
    await stack.shutdown();
  }
  for (const leak of stack.leaks) outcome.checks.push({ name: 'nothing outlives shutdown', ok: false, detail: { leak } });
  const failed = outcome.checks.filter((c) => !c.ok && (has('strict') || !c.xfail));
  const report = { commit: gitDescribe(stack.repo), date: new Date().toISOString(), ...outcome };
  const json = JSON.stringify(report, null, 2);
  if (opt('out')) fs.writeFileSync(opt('out'), `${json}\n`);
  console.log(json);
  const xfails = outcome.checks.filter((c) => !c.ok && c.xfail).length;
  log(`${outcome.checks.length - failed.length - (has('strict') ? 0 : xfails)} ok, ${xfails} xfail, ${failed.length} failed`);
  process.exit(failed.length ? 1 : 0);
}
