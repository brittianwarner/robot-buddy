// The local voice stack shared by tests/bench-e2e.mjs, tests/robot-reconnect.mjs and
// tests/eval-voice.mjs: the real server binary and its engine, the real robot client in
// --simulate mode, a fake streaming model, a fake speech provider and a fake aplay.
//
// Fakes (all on 127.0.0.1, test-only credentials):
//   model  POST /v1/chat/completions  SSE: first token at 250 ms, then 4 chars every 15 ms
//   speech POST /v1/audio/speech      audio/pcm 24 kHz mono s16: 300 ms to first byte, then
//                                      real time (48 kB/s), 60 ms of audio per character.
//                                      Every byte of a response is its request's tag
//                                      (1 to 255), so audio at the player names the
//                                      sentence it came from.
//   aplay  on PATH (tests/lib/fake-aplay.mjs) reports every PCM chunk, its arrival time
//          and its first byte (the tag)
//
// The stack refuses to start if any of its ports is in use; `shutdown()` reports anything
// that outlives it in `leaks`. Every port, the robot's loopback panel included
// (BUDDY_SCREEN_ADDR), comes from the port base, so stacks with different bases can run
// side by side. A `repo` whose robot predates BUDDY_SCREEN_ADDR keeps its panel on 8090.
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { startFaultProxy } from './fault-proxy.mjs';
import { cleanEnv as withoutLocalConfig } from './clean-env.mjs';

export const now = () => performance.timeOrigin + performance.now();
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
export const TERMINAL = new Set(['turn.completed', 'turn.failed', 'turn.cancelled', 'turn.interrupted']);
const root = path.resolve(import.meta.dir, '../..');
const TTS_KEY = 'bench-speech-key-not-real';
const MS_PER_CHAR = 60;
const CHUNK_MS = 20;
export const BYTES_PER_MS = 48; // 24 kHz * 2 bytes * 1 channel

export const SHORT_REPLIES = [
  'Sure, I can help with that. The kettle takes about three minutes.',
  'Good morning to you too. It looks like a calm day. Want me to set a reminder?',
  'That sounds like a plan. I will keep it in mind. Let me know when you are ready.',
  'Happy to. Your desk lamp is on. The window is still open.',
];
export const LONG_REPLY = Array.from(
  { length: 12 },
  (_, i) => `This is sentence number ${i + 1} of a long story about a small robot on a desk.`,
).join(' ');

export function listeners(port) {
  const r = spawnSync('lsof', ['-nP', `-iTCP:${port}`, '-sTCP:LISTEN', '-Fpc'], { encoding: 'utf8' });
  const lines = (r.stdout || '').split('\n');
  const found = [];
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].startsWith('p')) found.push({ pid: Number(lines[i].slice(1)), cmd: (lines[i + 1] || '').slice(1) });
  }
  return found;
}

export function stats(values) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return { n: 0 };
  // Nearest rank: with n = 20, p95 is the 19th value and p99 the 20th (the max).
  const at = (p) => v[Math.min(v.length - 1, Math.ceil((p / 100) * v.length) - 1)];
  const r = (x) => Math.round(x * 10) / 10;
  return { n: v.length, p50: r(at(50)), p95: r(at(95)), p99: r(at(99)), max: r(v[v.length - 1]) };
}

/** Silences while a reply plays: a chunk arriving after the buffered audio ran out. */
export function silences(player, floorMs = 20) {
  const perMs = (player.rate * player.channels * 2) / 1000;
  let clock = null;
  const gaps = [];
  for (const [t, n] of player.chunks) {
    if (clock !== null && t - clock >= floorMs) gaps.push(t - clock);
    clock = Math.max(clock ?? t, t) + n / perMs;
  }
  return gaps;
}

export function gitDescribe(repo) {
  return spawnSync('git', ['-C', repo, 'describe', '--always', '--dirty'], { encoding: 'utf8' }).stdout.trim();
}

function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export function rss(pid) {
  if (!pid) return null;
  const r = spawnSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' });
  const kb = Number(r.stdout.trim());
  return Number.isFinite(kb) && kb > 0 ? kb : null;
}

/**
 * Starts the stack and returns its handle. Options:
 *   repo       checkout whose release binaries run (default: this one)
 *   base       port base (BENCH_PORT_BASE, default 17500)
 *   build      run `cargo build --release` first (default true)
 *   speech     fake the speech provider (default true; off when the server has no TTS_BASE_URL)
 *   proxy      put tests/lib/fault-proxy.mjs between the robot and the engine
 *   reply      (lastUserText) => reply text or null for the default replies
 *   keep       keep the work directory
 *   log        (...args) => void
 */
export async function startStack(options = {}) {
  const repo = path.resolve(options.repo ?? root);
  const base = Number(options.base ?? process.env.BENCH_PORT_BASE ?? 17500);
  const log = options.log ?? ((...a) => console.error('stack:', ...a));
  const screenSource = path.join(repo, 'apps/robot/src/screen.rs');
  const screenMovable = fs.existsSync(screenSource) && fs.readFileSync(screenSource, 'utf8').includes('BUDDY_SCREEN_ADDR');
  const PORTS = {
    engine: base + 20, // + 1 api_peer, + 10 metrics
    server: base + 80,
    model: base + 91,
    speech: base + 92,
    proxy: base + 93,
    screen: screenMovable ? base + 94 : 8090,
  };
  const allPorts = [
    PORTS.engine,
    PORTS.engine + 1,
    PORTS.engine + 10,
    PORTS.server,
    PORTS.model,
    PORTS.speech,
    PORTS.proxy,
    PORTS.screen,
  ];
  const busy = allPorts.flatMap((p) => listeners(p).map((who) => `port ${p}: pid ${who.pid} ${who.cmd}`));
  if (busy.length) {
    const error = new Error(`ports in use:\n  ${busy.join('\n  ')}`);
    error.code = 'PORTS_BUSY';
    throw error;
  }

  const lock = fs.readFileSync(path.join(repo, 'Cargo.lock'), 'utf8');
  const rivetVersion = /name = "rivetkit"\nversion = "([^"]+)"/.exec(lock)?.[1];
  const speechSource = ['apps/server/src/speech.rs', 'apps/server/src/speech/provider.rs']
    .map((f) => path.join(repo, f))
    .filter((f) => fs.existsSync(f))
    .map((f) => fs.readFileSync(f, 'utf8'))
    .join('\n');
  // The speech provider can only be faked where the server reads TTS_BASE_URL; a server that
  // hardcodes the OpenRouter host must never get TTS_MODEL (it would call the real service).
  const SPEECH = options.speech !== false && speechSource.includes('TTS_BASE_URL');

  const engineBinary = () => {
    for (const v of ['RIVET_ENGINE_BINARY_PATH', 'BUDDY_ENGINE_BINARY']) {
      if (process.env[v] && fs.existsSync(process.env[v])) return process.env[v];
    }
    const target =
      process.platform === 'darwin'
        ? `${os.arch() === 'arm64' ? 'aarch64' : 'x86_64'}-apple-darwin`
        : `${os.arch() === 'arm64' ? 'aarch64' : 'x86_64'}-unknown-linux-musl`;
    const rel = path.join('.rivetkit/engine/v3', rivetVersion, `rivet-engine-${target}`);
    for (const r of [process.env.RIVETKIT_STORAGE_PATH, os.homedir(), '/tmp/robot-buddy-integration']) {
      if (r && fs.existsSync(path.join(r, rel))) return path.join(r, rel);
    }
    throw new Error(
      `no rivet-engine ${rivetVersion}; run bun run test:integration once (it downloads it) or set RIVET_ENGINE_BINARY_PATH`,
    );
  };
  const ENGINE = engineBinary();

  if (options.build !== false) {
    log(`building release binaries in ${repo}`);
    const b = spawnSync('cargo', ['build', '--release', '--locked', '-p', 'robot-buddy-server', '-p', 'robot-buddy-robot'], {
      cwd: repo,
      stdio: ['ignore', 'inherit', 'inherit'],
    });
    if (b.status !== 0) throw new Error(`cargo build exited ${b.status}`);
  }
  const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repo, 'target');
  const SERVER = path.join(targetDir, 'release/robot-buddy-server');
  const ROBOT = path.join(targetDir, 'release/robot-buddy-robot');

  // -------------------------------------------------------------------------------------
  // Fakes
  // -------------------------------------------------------------------------------------
  const model = { calls: 0, open: new Map(), aborted: [], streams: [], late: [] };
  let replyIndex = 0;
  const chunkFrame = (delta, finish) => {
    const chunk = {
      id: 'chatcmpl-bench',
      object: 'chat.completion.chunk',
      created: 1,
      model: 'bench',
      choices: [{ index: 0, delta, finish_reason: finish ?? null }],
    };
    if (finish) chunk.usage = { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 };
    return `data: ${JSON.stringify(chunk)}\n\n`;
  };
  const replyFor = (lastUser) => {
    const text = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser ?? '');
    const custom = options.reply?.(text);
    if (custom) return custom;
    return text.includes('long story') ? LONG_REPLY : SHORT_REPLIES[replyIndex++ % SHORT_REPLIES.length];
  };
  const modelServer = Bun.serve({
    hostname: '127.0.0.1',
    port: PORTS.model,
    idleTimeout: 120,
    async fetch(req) {
      if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/chat/completions')) {
        return new Response('not found', { status: 404 });
      }
      const body = await req.json();
      model.calls += 1;
      const lastUser = [...(body.messages ?? [])].reverse().find((m) => m.role === 'user');
      const reply = replyFor(lastUser);
      if (!body.stream) {
        return Response.json({
          id: 'chatcmpl-bench',
          object: 'chat.completion',
          created: 1,
          model: 'bench',
          choices: [{ index: 0, message: { role: 'assistant', content: reply }, finish_reason: 'stop' }],
          usage: { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 },
        });
      }
      const id = model.calls;
      const frames = [];
      for (let i = 0; i < reply.length; i += 4) frames.push(chunkFrame({ role: 'assistant', content: reply.slice(i, i + 4) }));
      frames.push(`${chunkFrame({}, 'stop')}data: [DONE]\n\n`);
      const start = now();
      const encoder = new TextEncoder();
      let k = 0;
      let cancelled = false;
      model.open.set(id, { start, reply });
      model.streams.push({ id, start, reply });
      const stream = new ReadableStream({
        async pull(controller) {
          // Absolute schedule: frame k at 250 + 15k ms, so timer drift does not accumulate.
          const wait = start + 250 + 15 * k - now();
          if (wait > 0) await sleep(wait);
          if (cancelled) return;
          // Like the speech fake: a frame 20 ms or more behind its schedule is host noise.
          const late = now() - (start + 250 + 15 * k);
          if (late >= 20) model.late.push({ t: now(), ms: late });
          controller.enqueue(encoder.encode(frames[k++]));
          if (k === frames.length) {
            model.open.delete(id);
            controller.close();
          }
        },
        cancel() {
          cancelled = true;
          model.open.delete(id);
          model.aborted.push({ id, t: now() });
        },
      });
      return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });
    },
  });

  const speech = { requests: [], aborted: [], late: [] };
  const speechServer = Bun.serve({
    hostname: '127.0.0.1',
    port: PORTS.speech,
    idleTimeout: 120,
    async fetch(req) {
      if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/audio/speech')) {
        return new Response('not found', { status: 404 });
      }
      if (req.headers.get('authorization') !== `Bearer ${TTS_KEY}`) return new Response('unauthorized', { status: 401 });
      const body = await req.json();
      const t = now();
      const input = String(body.input ?? '');
      const ms = Math.max(CHUNK_MS, input.length * MS_PER_CHAR);
      const total = Math.round(ms * BYTES_PER_MS) & ~1;
      const tag = (speech.requests.length % 255) + 1;
      const record = { t, tag, input, chars: input.length, bytes: total, format: body.response_format };
      speech.requests.push(record);
      await sleep(300 - (now() - t));
      const start = now();
      const step = CHUNK_MS * BYTES_PER_MS;
      let sent = 0;
      let k = 0;
      let cancelled = false;
      const stream = new ReadableStream({
        async pull(controller) {
          // Real time: chunk k leaves at start + 20k ms.
          const wait = start + CHUNK_MS * k - now();
          if (wait > 0) await sleep(wait);
          if (cancelled) return;
          // The fake itself fell behind real time (a loaded machine): such a gap is ours.
          const late = now() - (start + CHUNK_MS * k);
          if (late >= 20) speech.late.push({ t: now(), ms: late });
          const n = Math.min(step, total - sent);
          controller.enqueue(new Uint8Array(n).fill(tag));
          sent += n;
          k += 1;
          if (sent >= total) controller.close();
        },
        cancel() {
          cancelled = true;
          speech.aborted.push({ t: now(), tag });
        },
      });
      return new Response(stream, { headers: { 'content-type': 'audio/pcm' } });
    },
  });

  // Every fake aplay process connects here and streams its chunk timestamps.
  const work = fs.mkdtempSync(path.join(os.tmpdir().length > 60 ? '/tmp' : os.tmpdir(), 'robot-buddy-bench-'));
  const APLAY_SOCK = path.join(work, 'aplay.sock');
  const players = [];
  const aplayServer = net.createServer((conn) => {
    const player = { start: null, chunks: [], tags: new Set(), eof: null, end: null, closed: null, total: 0, rate: 24000, channels: 1 };
    players.push(player);
    readline.createInterface({ input: conn }).on('line', (line) => {
      const m = JSON.parse(line);
      if (m.type === 'start') Object.assign(player, { start: m.t, rate: m.rate, channels: m.channels, pid: m.pid });
      else if (m.type === 'chunk') {
        player.chunks.push([m.t, m.n, m.b]);
        if (m.b) player.tags.add(m.b);
        player.total += m.n;
      } else if (m.type === 'eof') player.eof = m.t;
      else if (m.type === 'end') player.end = m.t;
      else if (m.type === 'term') player.term = m.t;
    });
    conn.on('close', () => {
      player.closed = now();
    });
  });
  await new Promise((r) => aplayServer.listen(APLAY_SOCK, r));
  const fakebin = path.join(work, 'bin');
  fs.mkdirSync(fakebin);
  fs.writeFileSync(
    path.join(fakebin, 'aplay'),
    `#!/bin/sh\nexec "${process.execPath}" "${path.join(root, 'tests/lib/fake-aplay.mjs')}" "$@"\n`,
    { mode: 0o755 },
  );
  // Start the fake player once before measuring. Its first launch from a fresh directory
  // (sh, then Bun loading fake-aplay.mjs) took 3 to 4 s in some runs, which showed up as
  // warm-up first audio; a real aplay is a native binary already on disk.
  {
    const t = now();
    const warm = spawn(path.join(fakebin, 'aplay'), [], {
      env: { ...process.env, BENCH_APLAY_SOCK: APLAY_SOCK },
      stdio: ['pipe', 'ignore', 'ignore'],
    });
    warm.stdin.end();
    await new Promise((r) => warm.on('exit', r));
    players.length = 0;
    log(`fake aplay first launch: ${Math.round(now() - t)} ms (not measured)`);
  }

  // -------------------------------------------------------------------------------------
  // Server, engine and robot
  // -------------------------------------------------------------------------------------
  const leaks = [];
  const findings = {};
  let serverProc;
  let robotProc;
  let enginePid = null;
  let proxy = null;

  const cleanEnv = (extra) => {
    const env = { ...withoutLocalConfig(), ...extra };
    for (const k of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']) delete env[k];
    for (const k of ['TTS_MODEL', 'TTS_BASE_URL', 'TTS_API_KEY', 'AI_API_KEY', 'OIDC_ISSUER', 'BUDDY_LAB_TOKEN']) {
      if (!(k in extra)) delete env[k];
    }
    env.NO_PROXY = env.no_proxy = '127.0.0.1,localhost';
    return env;
  };
  const run = (cmd, args) => {
    const r = spawnSync(cmd, args, { encoding: 'utf8' });
    if (r.status !== 0) throw new Error(`${path.basename(cmd)} ${args[0]} failed: ${r.stderr}`);
  };

  const stop = async (proc, name, ms) => {
    if (!proc || proc.exitCode !== null || proc.signalCode !== null) return null;
    const t = now();
    proc.kill('SIGTERM');
    const deadline = Date.now() + ms;
    while (proc.exitCode === null && proc.signalCode === null && Date.now() < deadline) await sleep(50);
    if (proc.exitCode === null && proc.signalCode === null) {
      leaks.push(`${name} ignored SIGTERM for ${ms} ms`);
      proc.kill('SIGKILL');
      return null;
    }
    return now() - t;
  };
  let stopped = false;
  async function shutdown() {
    if (stopped) return;
    stopped = true;
    // stdin stays open, as on the device where it is the speech-recognition pipe.
    const robotExit = await stop(robotProc, 'robot', 10000);
    if (robotExit != null) findings.robot_sigterm_exit_ms = Math.round(robotExit);
    await stop(serverProc, 'server', 45000);
    if (enginePid) {
      const deadline = Date.now() + 15000;
      while (pidAlive(enginePid) && Date.now() < deadline) await sleep(100);
      if (pidAlive(enginePid)) {
        leaks.push(`engine ${enginePid} outlived the server`);
        process.kill(enginePid, 'SIGKILL');
      }
    }
    proxy?.close();
    modelServer.stop(true);
    speechServer.stop(true);
    aplayServer.close();
    for (const p of allPorts) for (const who of listeners(p)) leaks.push(`port ${p} still served by pid ${who.pid} ${who.cmd}`);
    if (!options.keep) fs.rmSync(work, { recursive: true, force: true });
  }

  const serverUrl = `http://127.0.0.1:${PORTS.server}`;
  const engineUrl = `http://127.0.0.1:${PORTS.engine}`;
  const events = [];
  const waiters = new Set();
  const onEvent = (e) => {
    events.push(e);
    for (const w of waiters) {
      if (w.match(e)) {
        waiters.delete(w);
        w.resolve(e);
      }
    }
  };
  function waitEvent(match, ms = 30000, what = 'event') {
    const seen = events.find(match);
    if (seen) return Promise.resolve(seen);
    return new Promise((resolve, reject) => {
      const w = { match, resolve };
      waiters.add(w);
      setTimeout(() => {
        if (waiters.delete(w)) reject(new Error(`timed out waiting for ${what}`));
      }, ms);
    });
  }
  async function until(what, fn, ms = 60000) {
    const deadline = Date.now() + ms;
    let last;
    while (Date.now() < deadline) {
      try {
        const v = await fn();
        if (v) return v;
      } catch (e) {
        last = e;
      }
      if (serverProc && serverProc.exitCode !== null) throw new Error(`server exited (${serverProc.exitCode}) while waiting for ${what}`);
      if (robotProc && robotProc.exitCode !== null) throw new Error(`robot exited (${robotProc.exitCode}) while waiting for ${what}`);
      await sleep(50);
    }
    throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ''}`);
  }

  try {
    run(SERVER, ['init', path.join(work, 'secrets')]);
    run(SERVER, ['provision', path.join(work, 'secrets/issuer.key'), path.join(work, 'device.json')]);
    const secrets = Object.fromEntries(
      fs
        .readFileSync(path.join(work, 'secrets/secrets.env'), 'utf8')
        .split('\n')
        .filter(Boolean)
        .map((l) => [l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)]),
    );
    const identity = JSON.parse(fs.readFileSync(path.join(work, 'device.json'), 'utf8'));
    if (options.proxy) proxy = await startFaultProxy({ listen: PORTS.proxy, upstream: PORTS.engine });
    const publicEngine = proxy ? `http://127.0.0.1:${PORTS.proxy}` : engineUrl;

    const serverLog = fs.openSync(path.join(work, 'server.log'), 'w');
    serverProc = spawn(SERVER, [], {
      env: cleanEnv({
        ...secrets,
        RUST_LOG: process.env.BENCH_SERVER_LOG ?? 'warn',
        RIVET_ENDPOINT: engineUrl,
        RIVETKIT_ENGINE_SPAWN: 'never',
        RIVET_NAMESPACE: 'default',
        RIVET_POOL: 'default',
        BUDDY_ENGINE_BINARY: ENGINE,
        BUDDY_ENGINE_PORT: String(PORTS.engine),
        BUDDY_DATA_DIR: path.join(work, 'data'),
        BUDDY_LISTEN: `127.0.0.1:${PORTS.server}`,
        BUDDY_PUBLIC_URL: serverUrl,
        BUDDY_PUBLIC_RIVET_URL: publicEngine,
        BUDDY_ALLOW_INSECURE_LOCAL: 'true',
        AI_PROVIDER: 'openai_compatible',
        AI_BASE_URL: `http://127.0.0.1:${PORTS.model}/v1`,
        AI_MODEL: 'bench-fixture',
        ...(SPEECH
          ? {
              TTS_MODEL: 'elevenlabs/eleven-v4-turbo',
              TTS_BASE_URL: `http://127.0.0.1:${PORTS.speech}/v1`,
              TTS_API_KEY: TTS_KEY,
            }
          : {}),
      }),
      stdio: ['ignore', serverLog, serverLog],
    });

    // Device enrollment and owner claim, as the robot and the /claim page would do them.
    const b64u = (buf) => Buffer.from(buf).toString('base64url');
    const PKCS8_ED25519 = Buffer.from('302e020100300506032b657004220420', 'hex');
    const deviceKey = crypto.createPrivateKey({
      key: Buffer.concat([PKCS8_ED25519, Buffer.from(identity.private_key, 'base64url')]),
      format: 'der',
      type: 'pkcs8',
    });
    const session = (bot, role) => {
      const body = b64u(JSON.stringify({ bot_id: bot, role, expires: Math.floor(Date.now() / 1000) + 600 }));
      const mac = crypto.createHmac('sha256', secrets.BUDDY_SIGNING_KEY).update(`session.${body}`).digest();
      return `${body}.${b64u(mac)}`;
    };
    const postJson = async (url, body) => {
      const r = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
      if (!r.ok) throw new Error(`${new URL(url).pathname} HTTP ${r.status}`);
      return r.json();
    };
    const enroll = async () => {
      const { challenge } = await postJson(`${serverUrl}/v1/challenge`, identity.certificate);
      const proof = b64u(crypto.sign(null, Buffer.from(challenge), deviceKey));
      return postJson(`${serverUrl}/v1/enroll`, { certificate: identity.certificate, challenge, proof });
    };
    const botCall = async (bundle, token, command) => {
      const actor = bundle.actor_id;
      // Straight to the engine: the fault proxy only sits on the robot's path.
      const r = await fetch(`${engineUrl}/gateway/${encodeURIComponent(actor)}@${encodeURIComponent(bundle.gateway_token)}/action/call`, {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'x-rivet-encoding': 'json',
          'x-rivet-conn-params': JSON.stringify({ session: token }),
          'x-rivet-target': 'actor',
          'x-rivet-actor': actor,
          'x-rivet-token': bundle.gateway_token,
          'x-rivet-namespace': bundle.namespace,
        },
        body: JSON.stringify({ args: [{ session: token, command }] }),
      });
      if (!r.ok) throw new Error(`bot ${command.type} HTTP ${r.status}`);
      return (await r.json()).output;
    };

    await until('server /health', async () => (await fetch(`${serverUrl}/health`)).ok);
    enginePid = (await until('engine listener', async () => listeners(PORTS.engine)[0]?.pid, 30000)) ?? null;
    const bundle = await until('enrollment', enroll);
    const device = bundle.session;
    const admin = session(bundle.bot_id, 'admin');
    const claim = await botCall(bundle, device, { type: 'begin_claim' });
    const proposed = await botCall(bundle, admin, { type: 'propose_claim', token: claim.token, owner: 'bench|owner' });
    await botCall(bundle, device, { type: 'confirm_claim', code: proposed.code });
    const snapshot = await botCall(bundle, device, { type: 'snapshot' });
    if (!snapshot.owner) throw new Error('claim did not set an owner');

    robotProc = spawn(ROBOT, ['--simulate'], {
      env: cleanEnv({
        RUST_LOG: process.env.BENCH_ROBOT_LOG ?? 'warn',
        PATH: `${fakebin}:${process.env.PATH}`,
        BENCH_APLAY_SOCK: APLAY_SOCK,
        BUDDY_SERVER_URL: serverUrl,
        BUDDY_IDENTITY_FILE: path.join(work, 'device.json'),
        BUDDY_CACHE_FILE: path.join(work, 'robot-cache.json'),
        BUDDY_EVENTS: 'full',
        ...(screenMovable ? { BUDDY_SCREEN_ADDR: `127.0.0.1:${PORTS.screen}` } : {}),
        ...(SPEECH ? { BUDDY_SPEECH: 'true' } : {}),
      }),
      stdio: ['pipe', 'pipe', fs.openSync(path.join(work, 'robot.log'), 'w')],
    });
    readline.createInterface({ input: robotProc.stdout }).on('line', (line) => {
      const t = now();
      if (!line.startsWith('{')) return;
      try {
        const e = JSON.parse(line);
        if (e.kind && e.turn_id) onEvent({ ...e, t });
      } catch {}
    });
    await until(
      'robot ready',
      async () => (await (await fetch(`http://127.0.0.1:${PORTS.screen}/`)).text()).includes('Ready'),
      30000,
    );
  } catch (e) {
    e.logs = tails(work);
    await shutdown();
    throw e;
  }

  /** Sends one control line to the robot: plain text is an utterance. */
  const control = (value) => {
    const t = now();
    robotProc.stdin.write(`${typeof value === 'string' ? value : JSON.stringify(value)}\n`);
    return t;
  };
  /** Speech requests (and so their tags) whose text contains `marker`. */
  const requestsFor = (marker, after = 0) => speech.requests.filter((q) => q.t >= after && q.input.includes(marker));
  /** Players that played audio of speech requests containing `marker`. */
  const playersFor = (marker) => {
    const tags = new Set(requestsFor(marker).map((q) => q.tag));
    return players.filter((p) => [...p.tags].some((tag) => tags.has(tag)));
  };

  return {
    repo,
    PORTS,
    SPEECH,
    rivetVersion,
    work,
    events,
    players,
    speech,
    model,
    proxy,
    leaks,
    findings,
    get serverPid() {
      return serverProc?.pid;
    },
    get enginePid() {
      return enginePid;
    },
    waitEvent,
    until,
    control,
    say: control,
    requestsFor,
    playersFor,
    shutdown,
    logs: () => tails(work),
  };
}

function tails(work) {
  return ['server.log', 'robot.log']
    .map((f) => path.join(work, f))
    .filter((p) => fs.existsSync(p))
    .map((p) => `--- ${path.basename(p)} (tail)\n${fs.readFileSync(p, 'utf8').split('\n').slice(-30).join('\n')}`)
    .join('\n');
}
