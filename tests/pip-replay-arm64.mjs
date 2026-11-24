#!/usr/bin/env bun
// ARM64 recording-replay end-to-end fixture using the ACTUAL compiled
// aarch64-unknown-linux-gnu `robot-buddy-robot` binary in hardware mode
// (NO `--simulate`), against real `robot-buddy-server` + real `rivet-engine`
// over TLS (`SSL_CERT_FILE`), with dual Linux PTYs for the ESP32-S3 face and
// Feetech STS3215 1 Mbaud neck bus, plus synthesized 16 kHz PCM recordings
// replayed through `arecord` -> `Alsa` -> `Vad` -> `whisper` -> `Policy`.
//
// No real hardware is touched. Runs natively on Linux aarch64 or inside a
// `linux/arm64` Docker container (default image: `robot-buddy:pi-build-tools`).
//
// Usage:
//   bun tests/pip-replay-arm64.mjs [--no-build] [--image <docker-image>] [--keep]
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { cleanEnv as withoutLocalConfig } from './lib/clean-env.mjs';
import { BYTES_PER_MS, listeners, LONG_REPLY, now, sleep } from './lib/stack.mjs';

const argv = process.argv.slice(2);
const has = (flag) => argv.includes(`--${flag}`);
const opt = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 ? argv[i + 1] : fallback;
};

const root = path.resolve(import.meta.dir, '..');
const base = Number(opt('base', process.env.PIP_REPLAY_PORT_BASE ?? 18600));
const useDocker = process.platform !== 'linux' || os.arch() !== 'arm64' || has('docker');
const dockerImage = opt('image', process.env.PIP_REPLAY_IMAGE ?? 'robot-buddy:pi-build-tools');
const keep = has('keep');
const log = (...args) => console.error('pip-replay-arm64:', ...args);

const PORTS = {
  engine: base + 20, // +1 api_peer, +10 metrics
  server: base + 80,
  model: base + 91,
  speech: base + 92,
  tlsServer: base + 95,
  tlsEngine: base + 96,
};
const allPorts = [
  PORTS.engine,
  PORTS.engine + 1,
  PORTS.engine + 10,
  PORTS.server,
  PORTS.model,
  PORTS.speech,
  PORTS.tlsServer,
  PORTS.tlsEngine,
];
const busy = allPorts.flatMap((p) => listeners(p).map((w) => `port ${p}: pid ${w.pid} ${w.cmd}`));
if (busy.length) {
  console.error(`pip-replay-arm64: refusing to start, ports in use:\n  ${busy.join('\n  ')}`);
  process.exit(2);
}

const lock = fs.readFileSync(path.join(root, 'Cargo.lock'), 'utf8');
const rivetVersion = /name = "rivetkit"\nversion = "([^"]+)"/.exec(lock)?.[1];
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
  throw new Error(`no rivet-engine ${rivetVersion}; run bun run test:integration once or set RIVET_ENGINE_BINARY_PATH`);
};
const ENGINE = engineBinary();
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(root, 'target');
const SERVER = path.join(targetDir, 'release/robot-buddy-server');
const ARM64_ROBOT = path.join(root, 'target/pi-image/robot-buddy-robot');
const NATIVE_ROBOT = path.join(targetDir, 'release/robot-buddy-robot');
const ROBOT_BIN = useDocker ? ARM64_ROBOT : fs.existsSync(ARM64_ROBOT) ? ARM64_ROBOT : NATIVE_ROBOT;

if (!has('no-build')) {
  if (!fs.existsSync(SERVER)) {
    log('building robot-buddy-server (release)');
    const b = spawnSync('cargo', ['build', '--release', '--locked', '-p', 'robot-buddy-server'], {
      cwd: root,
      stdio: 'inherit',
    });
    if (b.status !== 0) process.exit(b.status ?? 1);
  }
  if (useDocker) {
    log('building ARM64 robot binary via infra/pi-build.Dockerfile');
    const b = spawnSync(
      'docker',
      ['buildx', 'build', '--platform', 'linux/arm64', '-f', 'infra/pi-build.Dockerfile', '--output', 'type=local,dest=target/pi-image', '.'],
      { cwd: root, stdio: 'inherit' },
    );
    if (b.status !== 0) process.exit(b.status ?? 1);
  } else if (!fs.existsSync(ROBOT_BIN)) {
    log('building native Linux ARM64 robot binary');
    const b = spawnSync('cargo', ['build', '--release', '--locked', '-p', 'robot-buddy-robot'], {
      cwd: root,
      stdio: 'inherit',
    });
    if (b.status !== 0) process.exit(b.status ?? 1);
  }
}
if (!fs.existsSync(SERVER)) throw new Error(`missing server binary: ${SERVER}`);
if (!fs.existsSync(ROBOT_BIN)) throw new Error(`missing ARM64 robot binary: ${ROBOT_BIN}`);

const work = fs.mkdtempSync(path.join('/tmp', 'pip-replay-arm64-'));
const queueDir = path.join(work, 'audio-queue');
const binDir = path.join(work, 'bin');
fs.mkdirSync(queueDir);
fs.mkdirSync(binDir);

// Generate ephemeral test-only CA and leaf cert covering host.docker.internal + 127.0.0.1.
const caKey = path.join(work, 'ca.key');
const caCert = path.join(work, 'ca.pem');
const srvKey = path.join(work, 'srv.key');
const srvCsr = path.join(work, 'srv.csr');
const srvCert = path.join(work, 'srv.pem');
const extFile = path.join(work, 'ext.cnf');
fs.writeFileSync(
  extFile,
  'basicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:host.docker.internal,DNS:localhost,IP:127.0.0.1\n',
);
const openssl = (args) => {
  const r = spawnSync('openssl', args, { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`openssl ${args[0]} failed: ${r.stderr}`);
};
openssl(['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-keyout', caKey, '-out', caCert, '-days', '1', '-subj', '/CN=PipReplayTestCA']);
openssl(['req', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-keyout', srvKey, '-out', srvCsr, '-subj', '/CN=host.docker.internal']);
openssl(['x509', '-req', '-in', srvCsr, '-CA', caCert, '-CAkey', caKey, '-CAcreateserial', '-out', srvCert, '-days', '1', '-extfile', extFile]);

const tlsOpts = {
  key: fs.readFileSync(srvKey),
  cert: fs.readFileSync(srvCert),
  ALPNProtocols: ['http/1.1'],
};
function startTlsProxy(listenPort, upstreamPort) {
  const server = tls.createServer(tlsOpts, (client) => {
    const upstream = net.connect(upstreamPort, '127.0.0.1');
    client.pipe(upstream);
    upstream.pipe(client);
    const closeBoth = () => {
      client.destroy();
      upstream.destroy();
    };
    client.on('error', closeBoth);
    upstream.on('error', closeBoth);
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(listenPort, '127.0.0.1', () => resolve(server));
  });
}

// Calibration matching scripts/pip-perf-pty-harness.c initial servo EEPROM memory.
fs.writeFileSync(
  path.join(work, 'neck.json'),
  JSON.stringify(
    {
      yaw: { id: 1, center: 2048, min_steps: 1888, max_steps: 2208, invert: false, max_speed_deg_s: 45, accel: 20 },
      pitch: { id: 2, center: 2048, min_steps: 1820, max_steps: 2276, invert: true, max_speed_deg_s: 45, accel: 20 },
      roll: { id: 3, center: 2048, min_steps: 1888, max_steps: 2208, invert: false, max_speed_deg_s: 45, accel: 20 },
    },
    null,
    2,
  ),
);
fs.writeFileSync(path.join(work, 'fake-ggml.bin'), 'pip-replay-fixture-model\n');

// Synthesize a 16 kHz mono S16_LE PCM recording with a known tone frequency.
function writePcmClip(name, freqHz, durationMs = 700, amp = 4500) {
  const rate = 16000;
  const lead = Math.round((rate * 120) / 1000);
  const body = Math.round((rate * durationMs) / 1000);
  const buf = Buffer.alloc((lead + body) * 2);
  for (let i = 0; i < body; i++) {
    const val = Math.round(amp * Math.sin((2 * Math.PI * freqHz * i) / rate));
    buf.writeInt16LE(val, (lead + i) * 2);
  }
  const tmp = path.join(queueDir, `.${name}.tmp`);
  const dst = path.join(queueDir, `${name}.pcm`);
  fs.writeFileSync(tmp, buf);
  fs.renameSync(tmp, dst);
}

// In-container/ARM64 Perl shims (zero external packages; works in robot-buddy:pi-build-tools).
const containerWork = useDocker ? '/work' : work;

fs.writeFileSync(
  path.join(binDir, 'arecord'),
  `#!/usr/bin/perl
use strict;
use warnings;
binmode(STDOUT);
select((select(STDOUT), $| = 1)[0]);
my $qdir = '${containerWork}/audio-queue';
my $frame = "\\0" x 640; # 20 ms at 16 kHz mono S16_LE
my $silence_tail = 55;   # 1.1 s trailing silence (> 800 ms VAD END_FRAMES)
my $fh;
my $tail = 0;
while (1) {
  if (!defined $fh && $tail == 0) {
    opendir(my $dh, $qdir);
    my @files = sort grep { /^[^.].*\\.pcm$/ } readdir($dh);
    closedir($dh);
    if (@files) {
      my $src = "$qdir/$files[0]";
      my $dst = "$qdir/.$files[0].playing";
      if (rename($src, $dst) && open(my $in, '<:raw', $dst)) {
        $fh = $in;
        unlink($dst);
      }
    }
  }
  my $out = $frame;
  if (defined $fh) {
    my $buf = '';
    my $n = read($fh, $buf, 640);
    if ($n && $n > 0) {
      $out = $buf . ("\\0" x (640 - $n));
    } else {
      close($fh);
      undef $fh;
      $tail = $silence_tail;
    }
  } elsif ($tail > 0) {
    $tail--;
  }
  print STDOUT $out or exit 0;
  select(undef, undef, undef, 0.02);
}
`,
  { mode: 0o755 },
);

fs.writeFileSync(
  path.join(binDir, 'whisper-replay'),
  `#!/usr/bin/perl
use strict;
use warnings;
binmode(STDIN);
local $/;
my $data = <STDIN> // '';
die "invalid WAV header" unless length($data) > 44 && substr($data, 0, 4) eq 'RIFF' && substr($data, 8, 4) eq 'WAVE';
my $pcm = substr($data, 44);
my @samples = unpack('s<*', $pcm);
my ($cross, $voiced) = (0, 0);
my $chunk = 160; # 10 ms at 16 kHz
for (my $i = 0; $i + $chunk <= @samples; $i += $chunk) {
  my $sq = 0;
  for (my $j = $i; $j < $i + $chunk; $j++) { $sq += $samples[$j] * $samples[$j]; }
  if (sqrt($sq / $chunk) > 800) {
    $voiced += $chunk;
    for (my $j = $i + 1; $j < $i + $chunk; $j++) {
      $cross++ if ($samples[$j - 1] <= 0 && $samples[$j] > 0);
    }
  }
}
my $secs = $voiced > 0 ? ($voiced / 16000.0) : 0.001;
my $freq = $cross / $secs;
my $text = '';
if    ($freq >= 360 && $freq <= 540)  { $text = 'Hey Pip, nod your head and look surprised.'; }
elsif ($freq >= 560 && $freq <= 760)  { $text = 'Hey Pip, wiggle your ears and tell me a long story.'; }
elsif ($freq >= 780 && $freq <= 980)  { $text = 'Hey Pip, stop talking.'; }
elsif ($freq >= 1000 && $freq <= 1250){ $text = 'Hey Pip, go to sleep.'; }
else                                  { $text = '[BLANK_AUDIO]'; }
if (open(my $lf, '>>', '${containerWork}/whisper.jsonl')) {
  printf $lf '{"t_s":%d,"wav_bytes":%d,"freq_hz":%.1f,"text":"%s"}' . "\\n", time(), length($data), $freq, $text;
  close($lf);
}
print "$text\\n";
`,
  { mode: 0o755 },
);

fs.writeFileSync(
  path.join(binDir, 'aplay'),
  `#!/usr/bin/perl
use strict;
use warnings;
binmode(STDIN);
my $logf = '${containerWork}/aplay.jsonl';
sub emit {
  my ($line) = @_;
  if (open(my $f, '>>', $logf)) {
    print $f "$line\\n";
    close($f);
  }
}
emit(sprintf('{"type":"start","pid":%d,"t_s":%d}', $$, time()));
my $total = 0;
my $buf = '';
while (my $n = sysread(STDIN, $buf, 4096)) {
  $total += $n;
  my $tag = unpack('C', $buf);
  emit(sprintf('{"type":"chunk","pid":%d,"bytes":%d,"tag":%d,"t_s":%d}', $$, $n, $tag, time()));
}
emit(sprintf('{"type":"eof","pid":%d,"total":%d,"t_s":%d}', $$, $total, time()));
select(undef, undef, undef, 0.1);
emit(sprintf('{"type":"end","pid":%d,"total":%d,"t_s":%d}', $$, $total, time()));
`,
  { mode: 0o755 },
);

const hostForRobot = useDocker ? 'host.docker.internal' : '127.0.0.1';
const publicServerUrl = `https://${hostForRobot}:${PORTS.tlsServer}`;
const publicEngineUrl = `https://${hostForRobot}:${PORTS.tlsEngine}`;
const localServerUrl = `http://127.0.0.1:${PORTS.server}`;
const localEngineUrl = `http://127.0.0.1:${PORTS.engine}`;
const TTS_KEY = 'pip-replay-speech-key-not-real';

// Mock model that emits real `express` tool calls on Turn 1 and Turn 2, then spoken replies.
const modelCalls = [];
const chunkFrame = (delta, finish) => {
  const chunk = {
    id: 'chatcmpl-replay',
    object: 'chat.completion.chunk',
    created: 1,
    model: 'replay',
    choices: [{ index: 0, delta, finish_reason: finish ?? null }],
  };
  if (finish) chunk.usage = { prompt_tokens: 4, completion_tokens: 4, total_tokens: 8 };
  return `data: ${JSON.stringify(chunk)}\n\n`;
};
const toolFrame = (id, name, args) =>
  `${chunkFrame({ role: 'assistant', tool_calls: [{ index: 0, id, type: 'function', function: { name, arguments: JSON.stringify(args) } }] }, 'tool_calls')}data: [DONE]\n\n`;

const modelServer = Bun.serve({
  hostname: '127.0.0.1',
  port: PORTS.model,
  idleTimeout: 120,
  async fetch(req) {
    if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/chat/completions')) {
      return new Response('not found', { status: 404 });
    }
    const body = await req.json();
    const msgs = body.messages ?? [];
    const lastUser = [...msgs].reverse().find((m) => m.role === 'user');
    const userText = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser ?? '');
    const hasTools = Array.isArray(body.tools) && body.tools.length > 0;
    const afterTool = msgs.some((m) => m.role === 'tool');
    modelCalls.push({ userText, hasTools, afterTool });

    if (hasTools && !afterTool && userText.toLowerCase().includes('nod')) {
      return new Response(
        toolFrame('call-nod', 'express', { emotion: 'surprised', intensity: 0.9, gesture: 'nod' }),
        { headers: { 'content-type': 'text/event-stream' } },
      );
    }
    if (hasTools && !afterTool && userText.toLowerCase().includes('wiggle')) {
      return new Response(
        toolFrame('call-wiggle', 'express', { emotion: 'happy', intensity: 0.85, gesture: 'ear_wiggle' }),
        { headers: { 'content-type': 'text/event-stream' } },
      );
    }
    if (userText.toLowerCase().includes('long story')) {
      // Stream LONG_REPLY sentence-by-sentence so the turn is still live while sentence 1 plays.
      const sentences = LONG_REPLY.split(/(?<=\.) /);
      const enc = new TextEncoder();
      let idx = 0;
      const stream = new ReadableStream({
        async pull(controller) {
          if (idx > 0) await sleep(350);
          if (idx < sentences.length) {
            const prefix = idx === 0 ? '' : ' ';
            controller.enqueue(enc.encode(chunkFrame({ role: 'assistant', content: `${prefix}${sentences[idx++]}` })));
          } else {
            controller.enqueue(enc.encode(`${chunkFrame({}, 'stop')}data: [DONE]\n\n`));
            controller.close();
          }
        },
      });
      return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });
    }
    const sse = `${chunkFrame({ role: 'assistant', content: 'Oh! That really surprised me.' })}${chunkFrame({}, 'stop')}data: [DONE]\n\n`;
    return new Response(sse, { headers: { 'content-type': 'text/event-stream' } });
  },
});

const speechRequests = [];
const speechServer = Bun.serve({
  hostname: '127.0.0.1',
  port: PORTS.speech,
  idleTimeout: 120,
  async fetch(req) {
    if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/audio/speech')) {
      return new Response('not found', { status: 404 });
    }
    if (req.headers.get('authorization') !== `Bearer ${TTS_KEY}`) {
      return new Response('unauthorized', { status: 401 });
    }
    const body = await req.json();
    const input = String(body.input ?? '');
    const tag = (speechRequests.length % 255) + 1;
    const totalMs = Math.max(400, input.length * 45);
    const totalBytes = Math.round(totalMs * BYTES_PER_MS) & ~1;
    speechRequests.push({ tag, input, totalBytes });
    const step = 20 * BYTES_PER_MS;
    let sent = 0;
    let cancelled = false;
    const start = now();
    let k = 0;
    const stream = new ReadableStream({
      async pull(controller) {
        const wait = start + 20 * k - now();
        if (wait > 0) await sleep(wait);
        if (cancelled) return;
        const n = Math.min(step, totalBytes - sent);
        controller.enqueue(new Uint8Array(n).fill(tag));
        sent += n;
        k += 1;
        if (sent >= totalBytes) controller.close();
      },
      cancel() {
        cancelled = true;
      },
    });
    return new Response(stream, { headers: { 'content-type': 'audio/pcm' } });
  },
});

const tlsServerProxy = await startTlsProxy(PORTS.tlsServer, PORTS.server);
const tlsEngineProxy = await startTlsProxy(PORTS.tlsEngine, PORTS.engine);

let serverProc;
let runnerProc;
let enginePid = null;

const cleanEnv = (extra) => {
  const env = { ...withoutLocalConfig(), ...extra };
  for (const k of ['HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy']) delete env[k];
  for (const k of ['TTS_MODEL', 'TTS_BASE_URL', 'TTS_API_KEY', 'AI_API_KEY', 'OIDC_ISSUER', 'BUDDY_LAB_TOKEN']) {
    if (!(k in extra)) delete env[k];
  }
  env.NO_PROXY = env.no_proxy = '127.0.0.1,localhost';
  return env;
};

const runSync = (cmd, args) => {
  const r = spawnSync(cmd, args, { encoding: 'utf8' });
  if (r.status !== 0) throw new Error(`${path.basename(cmd)} ${args[0]} failed: ${r.stderr}`);
};

const readJsonl = (filePath) => {
  if (!fs.existsSync(filePath)) return [];
  return fs
    .readFileSync(filePath, 'utf8')
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      try {
        return JSON.parse(line);
      } catch {
        return null;
      }
    })
    .filter(Boolean);
};

async function until(what, fn, ms = 30000) {
  const deadline = Date.now() + ms;
  let lastErr;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      lastErr = e;
    }
    if (serverProc && serverProc.exitCode !== null) {
      throw new Error(`server exited (${serverProc.exitCode}) while waiting for ${what}`);
    }
    if (runnerProc && runnerProc.exitCode !== null) {
      throw new Error(`ARM64 runner exited (${runnerProc.exitCode}) while waiting for ${what}`);
    }
    await sleep(50);
  }
  throw new Error(`timed out waiting for ${what}${lastErr ? `: ${lastErr.message}` : ''}`);
}

const stopProc = async (proc, ms = 10000) => {
  if (!proc || proc.exitCode !== null || proc.signalCode !== null) return;
  proc.kill('SIGTERM');
  const deadline = Date.now() + ms;
  while (proc.exitCode === null && proc.signalCode === null && Date.now() < deadline) await sleep(50);
  if (proc.exitCode === null && proc.signalCode === null) proc.kill('SIGKILL');
};

try {
  runSync(SERVER, ['init', path.join(work, 'secrets')]);
  runSync(SERVER, ['provision', path.join(work, 'secrets/issuer.key'), path.join(work, 'device.json')]);
  const secrets = Object.fromEntries(
    fs
      .readFileSync(path.join(work, 'secrets/secrets.env'), 'utf8')
      .split('\n')
      .filter(Boolean)
      .map((l) => [l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)]),
  );
  const identity = JSON.parse(fs.readFileSync(path.join(work, 'device.json'), 'utf8'));

  const serverLog = fs.openSync(path.join(work, 'server.log'), 'w');
  serverProc = spawn(SERVER, [], {
    env: cleanEnv({
      ...secrets,
      RUST_LOG: 'warn',
      RIVET_ENDPOINT: localEngineUrl,
      RIVETKIT_ENGINE_SPAWN: 'never',
      RIVET_NAMESPACE: 'default',
      RIVET_POOL: 'default',
      BUDDY_ENGINE_BINARY: ENGINE,
      BUDDY_ENGINE_PORT: String(PORTS.engine),
      BUDDY_DATA_DIR: path.join(work, 'data'),
      BUDDY_LISTEN: `127.0.0.1:${PORTS.server}`,
      BUDDY_PUBLIC_URL: publicServerUrl,
      BUDDY_PUBLIC_RIVET_URL: publicEngineUrl,
      BUDDY_ALLOW_INSECURE_LOCAL: 'true',
      AI_PROVIDER: 'openai_compatible',
      AI_BASE_URL: `http://127.0.0.1:${PORTS.model}/v1`,
      AI_MODEL: 'replay-fixture',
      TTS_MODEL: 'elevenlabs/eleven-v4-turbo',
      TTS_BASE_URL: `http://127.0.0.1:${PORTS.speech}/v1`,
      TTS_API_KEY: TTS_KEY,
    }),
    stdio: ['ignore', serverLog, serverLog],
  });

  // Enroll & claim device directly on local server + engine (same as tests/lib/stack.mjs).
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
    const { challenge } = await postJson(`${localServerUrl}/v1/challenge`, identity.certificate);
    const proof = b64u(crypto.sign(null, Buffer.from(challenge), deviceKey));
    return postJson(`${localServerUrl}/v1/enroll`, { certificate: identity.certificate, challenge, proof });
  };
  const botCall = async (bundle, token, command) => {
    const actor = bundle.actor_id;
    const r = await fetch(`${localEngineUrl}/gateway/${encodeURIComponent(actor)}@${encodeURIComponent(bundle.gateway_token)}/action/call`, {
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

  await until('server /ready', async () => (await fetch(`${localServerUrl}/ready`)).ok, 30000);
  enginePid = (await until('engine listener', async () => listeners(PORTS.engine)[0]?.pid, 30000)) ?? null;
  let bundle = null;
  for (let attempt = 0; attempt < 20; attempt++) {
    try {
      bundle = await enroll();
      break;
    } catch {
      await sleep(500);
    }
  }
  if (!bundle) throw new Error('timed out waiting for device enrollment');
  const admin = session(bundle.bot_id, 'admin');
  const claim = await botCall(bundle, bundle.session, { type: 'begin_claim' });
  const proposed = await botCall(bundle, admin, { type: 'propose_claim', token: claim.token, owner: 'replay|owner' });
  await botCall(bundle, bundle.session, { type: 'confirm_claim', code: proposed.code });
  log('server + engine ready and device claimed');

  // Stage ARM64 binary + C PTY harness inside workdir so container mount is self-contained.
  fs.copyFileSync(path.join(root, 'scripts/pip-perf-pty-harness.c'), path.join(work, 'pty-harness.c'));
  fs.copyFileSync(ROBOT_BIN, path.join(work, 'robot-buddy-robot'));
  fs.chmodSync(path.join(work, 'robot-buddy-robot'), 0o755);

  const runnerScript = path.join(work, 'run-arm64.sh');
  fs.writeFileSync(
    runnerScript,
    `#!/bin/sh
set -eu
W="${containerWork}"
uname -m > "$W/arch.txt"
cc -O2 -Wall -Wextra -Werror -pthread "$W/pty-harness.c" -o /tmp/pty-harness
/tmp/pty-harness "$W/pty-ready.json" "$W/face.jsonl" "$W/neck.jsonl" &
HPID=$!
for i in $(seq 1 100); do
  [ -f "$W/pty-ready.json" ] && break
  sleep 0.05
done
FACE_PTY="$(perl -ne 'print $1 if /"face_pty":"([^"]+)"/' "$W/pty-ready.json")"
NECK_PTY="$(perl -ne 'print $1 if /"neck_pty":"([^"]+)"/' "$W/pty-ready.json")"
cp "$W/device.json" /tmp/device.identity.json
chmod 0600 /tmp/device.identity.json
rm -f /tmp/robot-control.sock
PATH="$W/bin:/usr/local/bin:/usr/bin:/bin" \
SSL_CERT_FILE="$W/ca.pem" \
BUDDY_SERVER_URL="${publicServerUrl}" \
BUDDY_IDENTITY_FILE="/tmp/device.identity.json" \
BUDDY_CACHE_FILE="/tmp/robot-cache.json" \
BUDDY_CONTROL_SOCKET="/tmp/robot-control.sock" \
BUDDY_SCREEN_ADDR="127.0.0.1:18090" \
BUDDY_SPEECH="true" \
BUDDY_FACE_SERIAL="$FACE_PTY" \
BUDDY_SERVO_PORT="$NECK_PTY" \
BUDDY_SERVO_CALIBRATION="$W/neck.json" \
BUDDY_LISTEN_MODEL="$W/fake-ggml.bin" \
BUDDY_LISTEN_WHISPER="$W/bin/whisper-replay" \
BUDDY_LISTEN_DEVICE="replay" \
BUDDY_LISTEN_CHANNELS="1" \
BUDDY_LISTEN_CHANNEL="0" \
RUST_LOG="info,robot_buddy_robot::audio=debug" \
"$W/robot-buddy-robot" > "$W/robot-stdout.log" 2> "$W/robot-stderr.log" &
RPID=$!
echo "$RPID" > "$W/robot.pid"
while [ ! -f "$W/stop-robot" ]; do
  if [ -f "$W/control-send.txt" ]; then
    mv "$W/control-send.txt" "$W/control-sending.txt"
    perl -MIO::Socket::UNIX -e 'my $s = IO::Socket::UNIX->new(Peer => "/tmp/robot-control.sock") or die $!; open my $f, "<", $ARGV[0] or die $!; local $/; my $d = <$f>; print $s $d; close $s;' "$W/control-sending.txt"
    rm -f "$W/control-sending.txt"
  fi
  if ! kill -0 "$RPID" 2>/dev/null; then
    wait "$RPID" || true
    echo "early-exit" > "$W/robot-exit.txt"
    kill -TERM "$HPID" 2>/dev/null || true
    wait "$HPID" || true
    exit 1
  fi
  sleep 0.05
done
kill -TERM "$RPID"
set +e
wait "$RPID"
RC=$?
set -e
echo "$RC" > "$W/robot-exit.txt"
kill -TERM "$HPID" 2>/dev/null || true
wait "$HPID" || true
exit 0
`,
    { mode: 0o755 },
  );

  const runnerLog = fs.openSync(path.join(work, 'runner.log'), 'w');
  if (useDocker) {
    log(`starting ARM64 Linux container (${dockerImage})`);
    runnerProc = spawn(
      'docker',
      [
        'run',
        '--rm',
        '--platform',
        'linux/arm64',
        '--add-host=host.docker.internal:host-gateway',
        '-v',
        `${work}:/work`,
        dockerImage,
        'sh',
        '/work/run-arm64.sh',
      ],
      { stdio: ['ignore', runnerLog, runnerLog] },
    );
  } else {
    log('running natively on Linux aarch64');
    runnerProc = spawn('sh', [runnerScript], { stdio: ['ignore', runnerLog, runnerLog] });
  }

  const faceLog = path.join(work, 'face.jsonl');
  const neckLog = path.join(work, 'neck.jsonl');
  const aplayLog = path.join(work, 'aplay.jsonl');
  const stdoutLog = path.join(work, 'robot-stdout.log');
  const stderrLog = path.join(work, 'robot-stderr.log');

  // Wait for ARM64 robot to arm the neck, open the face PTY, and reach Idle (connected + owned).
  await until('face PTY termios + idle mood', () => {
    const f = readJsonl(faceLog);
    const hasTermios = f.some((e) => e.type === 'termios');
    const hasIdle = f.some((e) => e.type === 'cmd' && e.cmd?.led === 'idle' && !f.every((x) => x.cmd?.e === 'sleepy'));
    const hasNeutral = f.some((e) => e.type === 'cmd' && e.cmd?.e === 'neutral');
    return hasTermios && hasIdle && hasNeutral;
  }, 30000);
  await until('neck PTY 1 Mbaud + torque enabled + 50 Hz goals', () => {
    const n = readJsonl(neckLog);
    const torqueOn = new Set(n.filter((e) => e.type === 'torque' && e.on === true).map((e) => e.id));
    const goals = n.filter((e) => e.type === 'goals').length;
    return torqueOn.size === 3 && goals >= 10;
  }, 20000);
  log('ARM64 robot online with Face PTY + STS3215 Neck PTY armed');

  const arch = fs.readFileSync(path.join(work, 'arch.txt'), 'utf8').trim();
  assert.equal(arch, 'aarch64', `expected aarch64 runtime, got ${arch}`);

  // ---- Turn 1: Recorded audio "Hey Pip, nod your head and look surprised." ----
  const neckBeforeTurn1 = readJsonl(neckLog).filter((e) => e.type === 'goals').length;
  log('replaying Turn 1 PCM recording (surprised + nod)');
  writePcmClip('01-surprised-nod', 440, 650);

  await until('Turn 1 face.express surprised + speaking + turn.completed', () => {
    const f = readJsonl(faceLog);
    const surprised = f.some((e) => e.type === 'cmd' && e.cmd?.e === 'surprised' && Math.abs(e.cmd.i - 0.9) < 0.01);
    const speaking = f.some((e) => e.type === 'cmd' && e.cmd?.led === 'speaking');
    const stdout = fs.existsSync(stdoutLog) ? fs.readFileSync(stdoutLog, 'utf8') : '';
    const completed = stdout.includes('kind=turn.completed');
    return surprised && speaking && completed;
  }, 25000);
  // Wait for Turn 1 playback to finish so robot returns to idle before Turn 2.
  await until('Turn 1 playback finished', () => readJsonl(aplayLog).some((e) => e.type === 'end'), 15000);
  await sleep(600);

  // Verify neck actually moved pitch away from 2048 during surprised + nod (within calibrated limits!).
  const turn1Goals = readJsonl(neckLog)
    .filter((e) => e.type === 'goals')
    .slice(neckBeforeTurn1);
  const maxPitchDeltaTurn1 = Math.max(...turn1Goals.map((g) => Math.abs(g.steps[1] - 2048)));
  assert.ok(
    maxPitchDeltaTurn1 >= 45,
    `expected significant pitch excursion on surprised+nod (>=45 steps), got ${maxPitchDeltaTurn1}`,
  );
  log(`Turn 1 verified: face surprised(0.9) + speaking, neck pitch excursion ${maxPitchDeltaTurn1} steps`);

  // ---- Turn 2: Recorded audio "Hey Pip, wiggle your ears and tell me a long story." + Barge-in ----
  const aplayStartsBefore = readJsonl(aplayLog).filter((e) => e.type === 'start').length;
  log('replaying Turn 2 PCM recording (ear_wiggle + long story)');
  writePcmClip('02-ear-wiggle-story', 660, 650);

  await until('Turn 2 ear_wiggle gesture + active playback chunks', () => {
    const f = readJsonl(faceLog);
    const wiggle = f.some((e) => e.type === 'cmd' && e.cmd?.g === 'ear_wiggle');
    const a = readJsonl(aplayLog);
    const starts = a.filter((e) => e.type === 'start');
    const secondPid = starts[aplayStartsBefore]?.pid;
    const chunks = secondPid ? a.filter((e) => e.type === 'chunk' && e.pid === secondPid).length : 0;
    return wiggle && chunks >= 5;
  }, 25000);

  const secondPlayerPid = readJsonl(aplayLog).filter((e) => e.type === 'start')[aplayStartsBefore].pid;
  log(`Turn 2 active (ear_wiggle seen, aplay pid ${secondPlayerPid} streaming); sending wake barge-in over control socket`);
  // Transcript mode intentionally disables mic capture during playback to prevent echo
  // self-interruption; barge-in without neural wake models enters via BUDDY_CONTROL_SOCKET.
  fs.writeFileSync(path.join(work, 'control-send.txt'), '{"wake":true}\n{"listening":false}\n');

  await until('Turn 2 cancelled via barge-in', () => {
    const stdout = fs.existsSync(stdoutLog) ? fs.readFileSync(stdoutLog, 'utf8') : '';
    return stdout.includes('kind=turn.cancelled');
  }, 20000);

  const aplayEventsForTurn2 = readJsonl(aplayLog).filter((e) => e.pid === secondPlayerPid);
  assert.equal(
    aplayEventsForTurn2.some((e) => e.type === 'end'),
    false,
    'barge-in must kill aplay before normal EOF drain',
  );
  log('Turn 2 verified: ear_wiggle emitted and barge-in killed playback & cancelled turn');

  // ---- Turn 3: Recorded audio "Hey Pip, go to sleep." ----
  await sleep(500);
  const neckBeforeSleep = readJsonl(neckLog).filter((e) => e.type === 'goals').length;
  log('replaying Turn 3 PCM recording (go to sleep)');
  writePcmClip('04-go-to-sleep', 1100, 650);

  await until('Face asleep + neck driven toward asleep pose', () => {
    const f = readJsonl(faceLog);
    const asleepFace = f.some((e) => e.type === 'cmd' && e.cmd?.e === 'asleep');
    const goals = readJsonl(neckLog).filter((e) => e.type === 'goals').slice(neckBeforeSleep);
    // pitch is inverted (invert=true) so -19.5 deg => steps > 2048 (+~222); roll -6 deg => steps < 2048 (-~68)
    const asleepNeck = goals.some((g) => g.steps[1] >= 2180 && g.steps[2] <= 2000);
    return asleepFace && asleepNeck;
  }, 20000);
  log('Turn 3 verified: face asleep + neck moved into calibrated asleep pose');

  // ---- Clean shutdown & torque-off verification ----
  fs.writeFileSync(path.join(work, 'stop-robot'), 'stop\n');
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timed out waiting for ARM64 runner shutdown')), 15000);
    runnerProc.once('exit', () => {
      clearTimeout(timer);
      resolve();
    });
  });

  const exitCode = fs.readFileSync(path.join(work, 'robot-exit.txt'), 'utf8').trim();
  assert.equal(exitCode, '0', `expected clean SIGTERM exit 0, got ${exitCode}`);

  // ---- Final assertions across captured PTY logs & motion instrumentation ----
  const faceEvents = readJsonl(faceLog);
  const neckEvents = readJsonl(neckLog);
  const robotLogText = `${fs.readFileSync(stdoutLog, 'utf8')}\n${fs.readFileSync(stderrLog, 'utf8')}`;

  // 1. Face PTY serial settings & shutdown reset.
  const faceTermios = faceEvents.find((e) => e.type === 'termios');
  assert.ok(faceTermios, 'missing face termios record');
  assert.equal(faceTermios.baud_115200, true, 'face PTY must be 115200 baud');
  assert.equal(faceTermios.clocal, true, 'face PTY must set CLOCAL');
  assert.equal(faceTermios.hupcl, false, 'face PTY must clear HUPCL');
  assert.equal(faceTermios.echo, false, 'face PTY must be raw (-echo)');
  assert.equal(faceTermios.icanon, false, 'face PTY must be raw (-icanon)');
  assert.equal(faceTermios.opost, false, 'face PTY must be raw (-opost)');
  assert.ok(faceEvents.some((e) => e.type === 'cmd' && e.cmd?.ping === 0), 'face must send ping on open');
  assert.ok(faceEvents.some((e) => e.type === 'cmd' && e.cmd?.led === 'listening'), 'face must show listening LED');
  assert.ok(faceEvents.some((e) => e.type === 'cmd' && e.cmd?.led === 'thinking'), 'face must show thinking LED');
  const lastFaceCmds = faceEvents.filter((e) => e.type === 'cmd').slice(-3);
  assert.ok(
    lastFaceCmds.some((e) => e.cmd?.e === 'sleepy' && e.cmd?.i === 0.5),
    `expected Offline/sleepy face on shutdown, got ${JSON.stringify(lastFaceCmds)}`,
  );

  // 2. Neck PTY 1 Mbaud termios, checksums, calibrated bounds, and torque-off on exit.
  const neckTermios = neckEvents.find((e) => e.type === 'termios');
  assert.ok(neckTermios, 'missing neck termios record');
  assert.equal(neckTermios.megabaud, true, 'neck PTY must be configured to Linux B1000000');
  assert.equal(neckTermios.clocal, true, 'neck PTY must set CLOCAL');
  assert.equal(neckTermios.hupcl, false, 'neck PTY must clear HUPCL');
  assert.equal(neckEvents.filter((e) => e.type === 'bad_checksum').length, 0, 'zero STS3215 checksum errors');

  const allGoals = neckEvents.filter((e) => e.type === 'goals');
  assert.ok(allGoals.length >= 100, `expected >=100 50 Hz goal frames, got ${allGoals.length}`);
  const mins = [1888, 1820, 1888];
  const maxs = [2208, 2276, 2208];
  for (const g of allGoals) {
    for (let i = 0; i < 3; i++) {
      assert.ok(
        g.steps[i] >= mins[i] && g.steps[i] <= maxs[i],
        `axis ${i} step ${g.steps[i]} outside [${mins[i]}, ${maxs[i]}]`,
      );
    }
  }
  const finalTorqueByServo = new Map();
  for (const e of neckEvents.filter((x) => x.type === 'torque')) {
    finalTorqueByServo.set(e.id, e.on);
  }
  for (const id of [1, 2, 3]) {
    assert.equal(finalTorqueByServo.get(id), false, `servo ${id} torque must be disabled on shutdown`);
  }

  // 3. Motion 50 Hz timing instrumentation present in ARM64 robot logs and parseable by pip-perf.py.
  assert.ok(
    robotLogText.includes('neck loop timing') || robotLogText.includes('neck loop missed 20ms deadline'),
    `expected motion timing instrumentation in ARM64 logs:\n${robotLogText.slice(-1500)}`,
  );
  const parsed = JSON.parse(
    spawnSync('python3', [path.join(root, 'scripts/pip-perf.py'), 'parse-journal', '-'], {
      input: robotLogText,
      encoding: 'utf8',
    }).stdout,
  );
  assert.ok(parsed.motion_20ms.total_ticks >= 100, `expected >=100 instrumented neck ticks, got ${parsed.motion_20ms.total_ticks}`);
  assert.equal(parsed.motion_20ms.neck_active, true);
  assert.equal(parsed.health.safety_fault_events, 0);

  log(
    `ALL ASSERTIONS PASSED (arch=${arch}, face_cmds=${faceEvents.filter((e) => e.type === 'cmd').length}, ` +
      `neck_goals=${allGoals.length}, neck_ticks=${parsed.motion_20ms.total_ticks}, ` +
      `late_cycles=${parsed.motion_20ms.late_cycles}, skipped_slots=${parsed.motion_20ms.skipped_slots})`,
  );
} catch (err) {
  console.error('pip-replay-arm64 FAILED:', err);
  for (const name of ['runner.log', 'robot-stderr.log', 'robot-stdout.log', 'server.log', 'whisper.jsonl']) {
    const p = path.join(work, name);
    if (fs.existsSync(p)) {
      console.error(`--- ${name} ---\n${fs.readFileSync(p, 'utf8').split('\n').slice(-40).join('\n')}`);
    }
  }
  process.exitCode = 1;
} finally {
  if (fs.existsSync(work)) {
    try {
      fs.writeFileSync(path.join(work, 'stop-robot'), 'stop\n');
    } catch {}
  }
  await stopProc(runnerProc, 8000);
  await stopProc(serverProc, 15000);
  if (enginePid) {
    try {
      process.kill(enginePid, 'SIGTERM');
    } catch {}
  }
  tlsServerProxy.close();
  tlsEngineProxy.close();
  modelServer.stop(true);
  speechServer.stop(true);
  if (!keep) fs.rmSync(work, { recursive: true, force: true });
}
