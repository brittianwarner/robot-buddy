// End-to-end latency bench: real server binary + its engine + the real robot client in
// --simulate mode, against a fake streaming model and a fake OpenRouter speech endpoint.
//
//   bun run bench:e2e                       # build release, 20 turns, 5 barge-ins
//   bun tests/bench-e2e.mjs --turns 50 --barge 10 --out /tmp/bench.json
//   bun tests/bench-e2e.mjs --repo ../rb-baseline --label baseline
//
// Fakes (all on 127.0.0.1, test-only credentials):
//   model  POST /v1/chat/completions  SSE: first token at 250 ms, then 4 chars every 15 ms
//   speech POST /v1/audio/speech      audio/pcm 24 kHz mono s16: 300 ms to first byte, then
//                                      real time (48 kB/s), 60 ms of audio per character
//   aplay  on PATH (tests/lib/fake-aplay.mjs) records arrival time of every PCM chunk
//
// Opt-in and local only. It refuses to start if any of its ports is in use, and it fails
// if the server, the engine or a listener is still alive after shutdown.
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';

// ---------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------
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
const out = opt('out', null);
const base = Number(process.env.BENCH_PORT_BASE ?? 17500);
const PORTS = {
  engine: base + 20, // + 1 api_peer, + 10 metrics
  server: base + 80,
  model: base + 91,
  speech: base + 92,
  screen: 8090, // fixed in apps/robot/src/screen.rs
};
const now = () => performance.timeOrigin + performance.now();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.error('bench:', ...a);

// ---------------------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------------------
function listeners(port) {
  const r = spawnSync('lsof', ['-nP', `-iTCP:${port}`, '-sTCP:LISTEN', '-Fpc'], { encoding: 'utf8' });
  const lines = (r.stdout || '').split('\n');
  const found = [];
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].startsWith('p')) found.push(`${lines[i].slice(1)} ${(lines[i + 1] || '').slice(1)}`);
  }
  return found;
}
const allPorts = () => [
  PORTS.engine, PORTS.engine + 1, PORTS.engine + 10, PORTS.server, PORTS.model, PORTS.speech, PORTS.screen,
];
const busy = allPorts().flatMap((p) => listeners(p).map((who) => `port ${p}: pid ${who}`));
if (busy.length) {
  log(`refusing to start, ports in use:\n  ${busy.join('\n  ')}`);
  process.exit(2);
}

const lock = fs.readFileSync(path.join(repo, 'Cargo.lock'), 'utf8');
const rivetVersion = /name = "rivetkit"\nversion = "([^"]+)"/.exec(lock)?.[1];
const commit = spawnSync('git', ['-C', repo, 'rev-parse', '--short', 'HEAD'], { encoding: 'utf8' }).stdout.trim();
const dirty = spawnSync('git', ['-C', repo, 'status', '--porcelain', '--', 'apps/*/src', 'crates/*/src', 'Cargo.lock'], {
  encoding: 'utf8',
}).stdout.trim() !== '';
const speechSource = ['apps/server/src/speech.rs', 'apps/server/src/speech/provider.rs']
  .map((f) => path.join(repo, f))
  .filter((f) => fs.existsSync(f))
  .map((f) => fs.readFileSync(f, 'utf8'))
  .join('\n');
// The speech provider can only be faked where the server reads TTS_BASE_URL; a server that
// hardcodes the OpenRouter host must never get TTS_MODEL (it would call the real service).
const SPEECH = !has('no-speech') && speechSource.includes('TTS_BASE_URL');

function engineBinary() {
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
  log(`no rivet-engine ${rivetVersion}; run bun run test:integration once (it downloads it) or set RIVET_ENGINE_BINARY_PATH`);
  process.exit(2);
}
const ENGINE = engineBinary();

if (!has('no-build')) {
  log(`building release binaries in ${repo}`);
  const b = spawnSync('cargo', ['build', '--release', '--locked', '-p', 'robot-buddy-server', '-p', 'robot-buddy-robot'], {
    cwd: repo,
    stdio: ['ignore', 'inherit', 'inherit'],
  });
  if (b.status !== 0) process.exit(b.status ?? 1);
}
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repo, 'target');
const SERVER = path.join(targetDir, 'release/robot-buddy-server');
const ROBOT = path.join(targetDir, 'release/robot-buddy-robot');

// ---------------------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------------------
const SHORT_REPLIES = [
  'Sure, I can help with that. The kettle takes about three minutes.',
  'Good morning to you too. It looks like a calm day. Want me to set a reminder?',
  'That sounds like a plan. I will keep it in mind. Let me know when you are ready.',
  'Happy to. Your desk lamp is on. The window is still open.',
];
const LONG_REPLY = Array.from(
  { length: 12 },
  (_, i) => `This is sentence number ${i + 1} of a long story about a small robot on a desk.`,
).join(' ');
const model = { calls: 0, open: new Map(), aborted: [] };
let replyIndex = 0;

function chunkFrame(delta, finish) {
  const chunk = {
    id: 'chatcmpl-bench',
    object: 'chat.completion.chunk',
    created: 1,
    model: 'bench',
    choices: [{ index: 0, delta, finish_reason: finish ?? null }],
  };
  if (finish) chunk.usage = { prompt_tokens: 3, completion_tokens: 2, total_tokens: 5 };
  return `data: ${JSON.stringify(chunk)}\n\n`;
}

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
    const reply = JSON.stringify(lastUser ?? '').includes('long story')
      ? LONG_REPLY
      : SHORT_REPLIES[replyIndex++ % SHORT_REPLIES.length];
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
    const stream = new ReadableStream({
      async pull(controller) {
        // Absolute schedule: frame k at 250 + 15k ms, so timer drift does not accumulate.
        const due = start + 250 + 15 * k;
        const wait = due - now();
        if (wait > 0) await sleep(wait);
        if (cancelled) return;
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
const MS_PER_CHAR = 60;
const CHUNK_MS = 20;
const BYTES_PER_MS = 48; // 24 kHz * 2 bytes * 1 channel

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
    const record = { t, chars: input.length, bytes: total, format: body.response_format };
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
        controller.enqueue(new Uint8Array(n));
        sent += n;
        k += 1;
        if (sent >= total) controller.close();
      },
      cancel() {
        cancelled = true;
        speech.aborted.push({ t: now() });
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
  const player = { start: null, chunks: [], eof: null, end: null, closed: null, total: 0, rate: 24000, channels: 1 };
  players.push(player);
  readline.createInterface({ input: conn }).on('line', (line) => {
    const m = JSON.parse(line);
    if (m.type === 'start') Object.assign(player, { start: m.t, rate: m.rate, channels: m.channels, pid: m.pid });
    else if (m.type === 'chunk') {
      player.chunks.push([m.t, m.n]);
      player.total += m.n;
    } else if (m.type === 'eof') player.eof = m.t;
    else if (m.type === 'end') player.end = m.t;
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

// ---------------------------------------------------------------------------------------
// Server, engine and robot
// ---------------------------------------------------------------------------------------
const TTS_KEY = 'bench-speech-key-not-real';
let serverProc;
let robotProc;
let enginePid;
const leaks = [];
const findings = {};

function cleanEnv(extra) {
  const env = { ...process.env, ...extra };
  for (const k of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']) delete env[k];
  for (const k of ['TTS_MODEL', 'TTS_BASE_URL', 'TTS_API_KEY', 'AI_API_KEY', 'OIDC_ISSUER', 'BUDDY_LAB_TOKEN']) {
    if (!(k in extra)) delete env[k];
  }
  env.NO_PROXY = env.no_proxy = '127.0.0.1,localhost';
  return env;
}

function run(cmd, args) {
  const r = spawnSync(cmd, args, { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`${path.basename(cmd)} ${args[0]} failed: ${r.stderr}`);
}
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
const serverUrl = `http://127.0.0.1:${PORTS.server}`;
const engineUrl = `http://127.0.0.1:${PORTS.engine}`;

const serverLog = fs.openSync(path.join(work, 'server.log'), 'w');
serverProc = spawn(SERVER, [], {
  env: cleanEnv({
    ...secrets,
    RUST_LOG: 'warn',
    RIVET_ENDPOINT: engineUrl,
    RIVETKIT_ENGINE_SPAWN: 'never',
    RIVET_NAMESPACE: 'default',
    RIVET_POOL: 'default',
    BUDDY_ENGINE_BINARY: ENGINE,
    BUDDY_ENGINE_PORT: String(PORTS.engine),
    BUDDY_DATA_DIR: path.join(work, 'data'),
    BUDDY_LISTEN: `127.0.0.1:${PORTS.server}`,
    BUDDY_PUBLIC_URL: serverUrl,
    BUDDY_PUBLIC_RIVET_URL: engineUrl,
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

function rss(pid) {
  if (!pid) return null;
  const r = spawnSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' });
  const kb = Number(r.stdout.trim());
  return Number.isFinite(kb) && kb > 0 ? kb : null;
}
function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
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
    if (serverProc.exitCode !== null) throw new Error(`server exited (${serverProc.exitCode}) while waiting for ${what}`);
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ''}`);
}

// Device enrollment and owner claim, as the robot and the /claim page would do them.
const b64u = (buf) => Buffer.from(buf).toString('base64url');
const PKCS8_ED25519 = Buffer.from('302e020100300506032b657004220420', 'hex');
const deviceKey = crypto.createPrivateKey({
  key: Buffer.concat([PKCS8_ED25519, Buffer.from(identity.private_key, 'base64url')]),
  format: 'der',
  type: 'pkcs8',
});
function session(bot, role) {
  const body = b64u(JSON.stringify({ bot_id: bot, role, expires: Math.floor(Date.now() / 1000) + 600 }));
  const mac = crypto.createHmac('sha256', secrets.BUDDY_SIGNING_KEY).update(`session.${body}`).digest();
  return `${body}.${b64u(mac)}`;
}
async function postJson(url, body) {
  const r = await fetch(url, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body) });
  if (!r.ok) throw new Error(`${new URL(url).pathname} HTTP ${r.status}`);
  return r.json();
}
async function enroll() {
  const { challenge } = await postJson(`${serverUrl}/v1/challenge`, identity.certificate);
  const proof = b64u(crypto.sign(null, Buffer.from(challenge), deviceKey));
  return postJson(`${serverUrl}/v1/enroll`, { certificate: identity.certificate, challenge, proof });
}
async function botCall(bundle, token, command) {
  const actor = bundle.actor_id;
  const r = await fetch(
    `${bundle.endpoint}/gateway/${encodeURIComponent(actor)}@${encodeURIComponent(bundle.gateway_token)}/action/call`,
    {
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
    },
  );
  if (!r.ok) throw new Error(`bot ${command.type} HTTP ${r.status}`);
  return (await r.json()).output;
}

// Robot stdout: event JSON lines (BUDDY_EVENTS=full), timestamped on arrival.
const events = [];
const waiters = new Set();
function onEvent(e) {
  events.push(e);
  for (const w of waiters) if (w.match(e)) {
    waiters.delete(w);
    w.resolve(e);
  }
}
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
const TERMINAL = new Set(['turn.completed', 'turn.failed', 'turn.cancelled', 'turn.interrupted']);

async function shutdown() {
  const stop = async (proc, name, ms) => {
    if (!proc || proc.exitCode !== null || proc.signalCode !== null) return;
    proc.kill('SIGTERM');
    const deadline = Date.now() + ms;
    while (proc.exitCode === null && proc.signalCode === null && Date.now() < deadline) await sleep(100);
    if (proc.exitCode === null && proc.signalCode === null) {
      leaks.push(`${name} ignored SIGTERM for ${ms} ms`);
      proc.kill('SIGKILL');
    }
  };
  if (robotProc && robotProc.exitCode === null && robotProc.signalCode === null) {
    // BUG (reported, not a leak): the robot does not exit on SIGTERM while its stdin is
    // open, because the runtime waits for the blocking stdin reader. On the device stdin
    // is the speech-recognition pipe. Measure it, then close stdin so it can exit.
    robotProc.kill('SIGTERM');
    const t = now();
    while (robotProc.exitCode === null && robotProc.signalCode === null && now() - t < 3000) await sleep(50);
    findings.robot_sigterm_with_open_stdin =
      robotProc.exitCode === null && robotProc.signalCode === null ? 'still running after 3000 ms' : `exited in ${Math.round(now() - t)} ms`;
    robotProc.stdin.end();
  }
  await stop(robotProc, 'robot', 10000);
  await stop(serverProc, 'server', 45000);
  if (enginePid) {
    const deadline = Date.now() + 15000;
    while (pidAlive(enginePid) && Date.now() < deadline) await sleep(100);
    if (pidAlive(enginePid)) {
      leaks.push(`engine ${enginePid} outlived the server`);
      process.kill(enginePid, 'SIGKILL');
    }
  }
  modelServer.stop(true);
  speechServer.stop(true);
  aplayServer.close();
  for (const p of allPorts()) for (const who of listeners(p)) leaks.push(`port ${p} still served by pid ${who}`);
  if (!has('keep')) fs.rmSync(work, { recursive: true, force: true });
}
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, async () => {
    log(`${sig}: shutting down`);
    await shutdown();
    process.exit(130);
  });
}

// ---------------------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------------------
function stats(values) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return { n: 0 };
  // Nearest rank: with n = 20, p95 is the 19th value and p99 the 20th (the max).
  const at = (p) => v[Math.min(v.length - 1, Math.ceil((p / 100) * v.length) - 1)];
  const r = (x) => Math.round(x * 10) / 10;
  return { n: v.length, p50: r(at(50)), p95: r(at(95)), p99: r(at(99)), max: r(v[v.length - 1]) };
}
/** Silences while a reply plays: a chunk arriving after the buffered audio ran out. */
function silences(player, floorMs = 20) {
  const perMs = (player.rate * player.channels * 2) / 1000;
  let clock = null;
  const gaps = [];
  for (const [t, n] of player.chunks) {
    if (clock !== null && t - clock >= floorMs) gaps.push(t - clock);
    clock = Math.max(clock ?? t, t) + n / perMs;
  }
  return gaps;
}

const result = {
  label,
  commit: commit + (dirty ? '+dirty' : ''),
  date: new Date().toISOString(),
  platform: `${os.platform()} ${os.arch()} ${os.cpus()[0]?.model ?? ''}`.trim(),
  rivetkit: rivetVersion,
  turns: TURNS,
  barge_ins: BARGES,
  speech_faked: SPEECH,
  note: SPEECH
    ? null
    : 'speech path not faked (the server has no TTS_BASE_URL); only text and speech.ready latency measured',
};

let exit = 0;
try {
  await until('server /health', async () => (await fetch(`${serverUrl}/health`)).ok);
  enginePid = Number(spawnSync('pgrep', ['-f', path.join(work, 'data')], { encoding: 'utf8' }).stdout.trim().split('\n')[0]) || null;
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
      RUST_LOG: 'warn',
      PATH: `${fakebin}:${process.env.PATH}`,
      BENCH_APLAY_SOCK: APLAY_SOCK,
      BUDDY_SERVER_URL: serverUrl,
      BUDDY_IDENTITY_FILE: path.join(work, 'device.json'),
      BUDDY_CACHE_FILE: path.join(work, 'robot-cache.json'),
      BUDDY_EVENTS: 'full',
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
  const say = (text) => {
    const t = now();
    robotProc.stdin.write(`${text}\n`);
    return t;
  };
  await until(
    'robot ready',
    async () => (await (await fetch(`http://127.0.0.1:${PORTS.screen}/`)).text()).includes('Ready'),
    30000,
  );

  const rssSamples = [];
  const sampler = setInterval(() => rssSamples.push([now(), rss(serverProc.pid), rss(enginePid)]), 250);

  /** One spoken turn; returns its timings relative to submit. */
  async function turn(text, { waitAudio = true } = {}) {
    const firstPlayer = players.length;
    const t0 = say(text);
    const started = await waitEvent((e) => e.kind === 'turn.started' && e.payload?.text === text && e.t >= t0, 30000, 'turn.started');
    const id = started.turn_id;
    const done = await waitEvent((e) => e.turn_id === id && TERMINAL.has(e.kind), 60000, 'turn end');
    let player = null;
    if (SPEECH && waitAudio) {
      await until('audio end', async () => players.slice(firstPlayer).find((p) => p.end || p.closed), 30000).catch(() => {});
      player = players.slice(firstPlayer).find((p) => p.chunks.length) ?? null;
    }
    const first = (kind) => events.find((e) => e.turn_id === id && e.kind === kind);
    const rel = (t) => (t == null ? null : t - t0);
    return {
      id,
      t0,
      kind: done.kind,
      started: rel(started.t),
      text: rel(first('text.delta')?.t),
      ready: rel(first('speech.ready')?.t),
      done: rel(done.t),
      pcm: rel(player?.chunks[0]?.[0]),
      gaps: player ? silences(player) : [],
      audio_ms: player ? player.total / ((player.rate * player.channels * 2) / 1000) : null,
    };
  }

  const cold = await turn('Hello there, this is the warm-up turn.');
  log(`warm-up: ${cold.kind}, speech.ready ${cold.ready?.toFixed(0)} ms, first pcm ${cold.pcm?.toFixed(0) ?? '-'} ms`);
  const rssStart = [rss(serverProc.pid), rss(enginePid)];
  const modelCallsBefore = model.calls;
  const speechBefore = speech.requests.length;
  const turns = [];
  for (let i = 0; i < TURNS; i++) {
    const r = await turn(`Bench turn ${i + 1}. Tell me something short.`);
    turns.push(r);
    log(`turn ${i + 1}/${TURNS}: ${r.kind} ready ${r.ready?.toFixed(0)} pcm ${r.pcm?.toFixed(0) ?? '-'} gaps [${r.gaps.map((g) => g.toFixed(0))}]`);
    await sleep(200);
  }
  const failed = turns.filter((r) => r.kind !== 'turn.completed');

  const barges = [];
  for (let i = 0; i < BARGES; i++) {
    const firstPlayer = players.length;
    const text = `Barge ${i + 1}: tell me a long story please.`;
    const t0 = say(text);
    const started = await waitEvent((e) => e.kind === 'turn.started' && e.payload?.text === text && e.t >= t0, 30000, 'barge turn');
    if (SPEECH) {
      await until('barge audio', async () => players.slice(firstPlayer).find((p) => p.chunks.length), 30000);
    } else {
      await waitEvent((e) => e.turn_id === started.turn_id && e.kind === 'speech.ready', 30000, 'barge speech.ready');
    }
    await sleep(500);
    const player = players.slice(firstPlayer).find((p) => p.chunks.length);
    const modelOpen = model.open.size > 0;
    const abortedBefore = model.aborted.length;
    const tb = say(`Stop. Barge reply ${i + 1}.`);
    const ended = await waitEvent((e) => e.turn_id === started.turn_id && TERMINAL.has(e.kind), 30000, 'barged turn end');
    if (player) await until('player closed', async () => player.closed, 10000).catch(() => {});
    await sleep(300);
    barges.push({
      end_kind: ended.kind,
      to_terminal_event: ended.t - tb,
      to_audio_closed: player?.closed ? player.closed - tb : null,
      to_provider_closed: modelOpen && model.aborted.length > abortedBefore ? model.aborted[abortedBefore].t - tb : null,
      provider_was_streaming: modelOpen,
    });
    log(`barge ${i + 1}/${BARGES}: ${ended.kind} event ${(ended.t - tb).toFixed(0)} ms, audio closed ${barges.at(-1).to_audio_closed?.toFixed(0) ?? '-'} ms`);
    // Let the replacement turn finish before the next trial.
    await waitEvent((e) => e.kind === 'turn.started' && e.payload?.text === `Stop. Barge reply ${i + 1}.`, 30000, 'replacement turn')
      .then((s) => waitEvent((e) => e.turn_id === s.turn_id && TERMINAL.has(e.kind), 60000, 'replacement end'));
    await until('players idle', async () => players.every((p) => p.closed), 30000).catch(() => {});
    await sleep(300);
  }
  clearInterval(sampler);

  const allGaps = turns.flatMap((r) => r.gaps);
  const server = rssSamples.map((s) => s[1]).filter(Boolean);
  const engine = rssSamples.map((s) => s[2]).filter(Boolean);
  Object.assign(result, {
    failed_turns: failed.length,
    cold_turn_ms: { speech_ready: cold.ready, first_pcm: cold.pcm, completed: cold.done },
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
      barge_in_to_audio_closed: stats(barges.map((b) => b.to_audio_closed)),
      barge_in_to_turn_terminal_event: stats(barges.map((b) => b.to_terminal_event)),
      barge_in_to_provider_stream_closed: stats(barges.map((b) => b.to_provider_closed)),
    },
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
    })),
    per_barge: barges.map((b) => ({
      end_kind: b.end_kind,
      audio_closed: b.to_audio_closed && Math.round(b.to_audio_closed * 10) / 10,
      terminal_event: Math.round(b.to_terminal_event),
      provider_closed: b.to_provider_closed && Math.round(b.to_provider_closed),
    })),
    model_calls_per_turn: (model.calls - modelCallsBefore) / (TURNS + 2 * BARGES),
    speech_requests_per_turn: SPEECH ? (speech.requests.length - speechBefore) / (TURNS + 2 * BARGES) : null,
    barge_end_kinds: [...new Set(barges.map((b) => b.end_kind))],
  });
  if (failed.length) {
    exit = 1;
    log(`${failed.length} turns did not complete: ${[...new Set(failed.map((r) => r.kind))]}`);
  }
} catch (e) {
  exit = 1;
  log(`FAILED: ${e.message}`);
  for (const f of ['server.log', 'robot.log']) {
    const p = path.join(work, f);
    if (fs.existsSync(p)) log(`--- ${f} (tail)\n${fs.readFileSync(p, 'utf8').split('\n').slice(-30).join('\n')}`);
  }
} finally {
  await shutdown();
}
if (leaks.length) {
  exit = 1;
  log(`LEAK:\n  ${leaks.join('\n  ')}`);
}
result.leaks = leaks;
result.findings = findings;

const json = JSON.stringify(result, null, 2);
if (out) fs.writeFileSync(out, `${json}\n`);
console.log(json);
const m = result.metrics_ms;
if (m) {
  const row = (name, s) =>
    `| ${name} | ${s.n ?? 0} | ${s.p50 ?? '-'} | ${s.p95 ?? '-'} | ${s.p99 ?? '-'} | ${s.max ?? '-'} |`;
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
process.exit(exit);
