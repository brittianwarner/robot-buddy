// ARM64 Pip E2E live stack controller.
//
// Starts the REAL release binaries (NO --simulate):
//   - robot-buddy-server + pinned real rivet-engine 3.1.1 (persistent /data dir)
//   - TLS-terminating reverse proxy for server HTTPS + TLS/WSS fault proxy for Rivet engine
//   - Mock HTTPS RS256 OIDC provider, mock streaming OpenAI model (with express tool), mock PCM speech
//   - Virtual Pip peripherals supervisor (tests/pip-e2e/peripherals/supervisor.py):
//     real Linux PTYs for ESP32 face + STS3215 servos, fake rpicam-vid, fake aplay/arecord,
//     slow-whisper wrapper around real ARM64 whisper-cli, optional fake nmcli
//   - Real robot-buddy-robot (--features pi-vision,onnx-wake) in hardware mode over real TLS
//
// Zero control-socket submit bypass: every speech turn enters through virtual arecord ->
// real VAD -> real ARM64 whisper-cli / neural wake -> real Rivet actor -> real speaker/face/servos.
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import readline from 'node:readline';
import {
  createTestTls,
  now,
  sleep,
  startMockModel,
  startMockOidc,
  startMockSpeech,
  startTlsFaultProxy,
} from './runner-fakes.mjs';

export const TERMINAL_KINDS = new Set([
  'turn.completed',
  'turn.failed',
  'turn.cancelled',
  'turn.interrupted',
]);

const root = path.resolve(import.meta.dir, '../..');
const b64u = (buf) => Buffer.from(buf).toString('base64url');
const ANSI_RE = /\x1b\[[0-9;]*m/g;

export function fileSha256(filePath) {
  if (!filePath || !fs.existsSync(filePath)) return null;
  const stat = fs.statSync(filePath);
  if (!stat.isFile()) return null;
  const hash = crypto.createHash('sha256');
  const fd = fs.openSync(filePath, 'r');
  try {
    const buf = Buffer.allocUnsafe(256 * 1024);
    for (;;) {
      const n = fs.readSync(fd, buf, 0, buf.length, null);
      if (n === 0) break;
      hash.update(buf.subarray(0, n));
    }
  } finally {
    fs.closeSync(fd);
  }
  return { path: filePath, bytes: stat.size, sha256: hash.digest('hex') };
}

/**
 * Computes a deterministic SHA-256 over product source files (apps/, crates/, Cargo.toml,
 * Cargo.lock, rust-toolchain.toml) so later doc/test-only commits can prove product-code
 * equivalence against an earlier qualifying soak.
 */
export function productSourceDigest(repoDir = root) {
  const files = [];
  const walk = (rel) => {
    const full = path.join(repoDir, rel);
    if (!fs.existsSync(full)) return;
    const st = fs.statSync(full);
    if (st.isFile()) {
      files.push(rel);
      return;
    }
    if (!st.isDirectory()) return;
    for (const entry of fs.readdirSync(full).sort()) {
      if (entry === 'target' || entry === '.git' || entry === 'node_modules') continue;
      walk(path.join(rel, entry));
    }
  };
  for (const top of ['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'apps', 'crates']) {
    walk(top);
  }
  files.sort();
  const h = crypto.createHash('sha256');
  for (const rel of files) {
    const full = path.join(repoDir, rel);
    h.update(rel);
    h.update('\0');
    h.update(fs.readFileSync(full));
    h.update('\0');
  }
  return { file_count: files.length, sha256: h.digest('hex') };
}

export function gitInfo(repoDir = root) {
  const runGit = (args) => {
    const r = spawnSync('git', ['-C', repoDir, ...args], { encoding: 'utf8' });
    return r.status === 0 ? r.stdout.trim() : null;
  };
  return {
    head: runGit(['rev-parse', 'HEAD']),
    branch: runGit(['rev-parse', '--abbrev-ref', 'HEAD']),
    describe: runGit(['describe', '--always', '--dirty']),
    dirty: Boolean(runGit(['status', '--porcelain'])),
  };
}

export function collectProvenance(paths, repoDir = root) {
  const engineVer = (() => {
    if (!paths.engineBin || !fs.existsSync(paths.engineBin)) return null;
    const r = spawnSync(paths.engineBin, ['--version'], { encoding: 'utf8' });
    return r.status === 0 ? r.stdout.trim() : null;
  })();
  const files = {};
  for (const [label, filePath] of Object.entries(paths)) {
    if (!filePath) continue;
    const info = fileSha256(filePath);
    if (info) {
      // Never emit host-specific private directories for optional private weights.
      files[label] = {
        name: path.basename(filePath),
        bytes: info.bytes,
        sha256: info.sha256,
      };
    } else {
      files[label] = { name: path.basename(filePath), missing: true };
    }
  }
  return {
    arch: `${process.platform}/${process.arch}`,
    engine_version: engineVer,
    git: gitInfo(repoDir),
    product_source_digest: productSourceDigest(repoDir),
    files,
  };
}

export function readProcMetrics(pid) {
  if (!pid) return null;
  try {
    process.kill(pid, 0);
  } catch {
    return null;
  }
  const statusPath = `/proc/${pid}/status`;
  if (!fs.existsSync(statusPath)) {
    const r = spawnSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' });
    const kb = Number(r.stdout.trim());
    return { pid, rss_kib: Number.isFinite(kb) ? kb : null, vm_size_kib: null, threads: null, fds: null };
  }
  const text = fs.readFileSync(statusPath, 'utf8');
  const num = (key) => {
    const m = new RegExp(`^${key}:\\s+(\\d+)`, 'm').exec(text);
    return m ? Number(m[1]) : null;
  };
  let fds = null;
  try {
    fds = fs.readdirSync(`/proc/${pid}/fd`).length;
  } catch {}
  return {
    pid,
    rss_kib: num('VmRSS'),
    vm_size_kib: num('VmSize'),
    threads: num('Threads'),
    fds,
  };
}

export function childPidsOf(parentPid) {
  if (!parentPid) return [];
  if (fs.existsSync(`/proc/${parentPid}/task`)) {
    const out = [];
    try {
      for (const tid of fs.readdirSync(`/proc/${parentPid}/task`)) {
        const p = `/proc/${parentPid}/task/${tid}/children`;
        if (!fs.existsSync(p)) continue;
        for (const tok of fs.readFileSync(p, 'utf8').trim().split(/\s+/)) {
          const n = Number(tok);
          if (Number.isFinite(n) && n > 0) out.push(n);
        }
      }
    } catch {}
    return [...new Set(out)];
  }
  const r = spawnSync('pgrep', ['-P', String(parentPid)], { encoding: 'utf8' });
  if (r.status !== 0) return [];
  return r.stdout
    .split(/\s+/)
    .map(Number)
    .filter((n) => Number.isFinite(n) && n > 0);
}

/** Walks all descendants of `rootPid` (children, grandchildren, e.g. whisper-slow -> whisper-cli). */
export function descendantPidsOf(rootPid) {
  if (!rootPid) return [];
  const visited = new Set();
  const queue = [...childPidsOf(rootPid)];
  while (queue.length > 0) {
    const pid = queue.shift();
    if (visited.has(pid)) continue;
    visited.add(pid);
    for (const c of childPidsOf(pid)) {
      if (!visited.has(c)) queue.push(c);
    }
  }
  return [...visited];
}

export function readDescendantMetrics(rootPid) {
  const pids = descendantPidsOf(rootPid);
  let totalRssKib = 0;
  let counted = 0;
  const byName = {};
  for (const pid of pids) {
    const m = readProcMetrics(pid);
    if (!m || m.rss_kib == null) continue;
    totalRssKib += m.rss_kib;
    counted += 1;
    const commPath = `/proc/${pid}/comm`;
    let comm = 'child';
    if (fs.existsSync(commPath)) {
      try {
        comm = fs.readFileSync(commPath, 'utf8').trim() || 'child';
      } catch {}
    }
    byName[comm] = (byName[comm] ?? 0) + m.rss_kib;
  }
  return {
    count: pids.length,
    measured_count: counted,
    rss_kib: counted > 0 ? totalRssKib : 0,
    by_comm_kib: byName,
  };
}

/** Reads Linux cgroup v2 memory counters if exposed in this container; otherwise explicit unknown. */
export function readCgroupMemory() {
  const readNum = (p) => {
    if (!fs.existsSync(p)) return null;
    try {
      const raw = fs.readFileSync(p, 'utf8').trim();
      if (raw === 'max') return null;
      const n = Number(raw);
      return Number.isFinite(n) ? n : null;
    } catch {
      return null;
    }
  };
  const currentBytes = readNum('/sys/fs/cgroup/memory.current');
  const peakBytes = readNum('/sys/fs/cgroup/memory.peak');
  const maxBytes = readNum('/sys/fs/cgroup/memory.max');
  let oomKill = null;
  if (fs.existsSync('/sys/fs/cgroup/memory.events')) {
    try {
      const txt = fs.readFileSync('/sys/fs/cgroup/memory.events', 'utf8');
      const m = /^oom_kill\s+(\d+)/m.exec(txt);
      if (m) oomKill = Number(m[1]);
    } catch {}
  }
  const available = currentBytes !== null || peakBytes !== null;
  return {
    available,
    current_mib: currentBytes != null ? Math.round((currentBytes / (1024 * 1024)) * 10) / 10 : null,
    peak_mib: peakBytes != null ? Math.round((peakBytes / (1024 * 1024)) * 10) / 10 : null,
    max_mib: maxBytes != null ? Math.round((maxBytes / (1024 * 1024)) * 10) / 10 : null,
    oom_kill: oomKill,
  };
}

/**
 * Parses robot.log and server.log for:
 *   - 50 Hz motion periodic timing windows (ticks, missed, skipped_slots, p95_cycle_us, max_overrun_us)
 *   - Audio first-PCM timing (headers_ms, first_pcm_ms)
 *   - Rust panics / fatal runtime errors
 *   - Wake mode startup / neural wake activity (counts only; no transcripts)
 */
export function parseLogTelemetry(workDir) {
  const readLog = (name) => {
    const p = path.join(workDir, name);
    return fs.existsSync(p) ? fs.readFileSync(p, 'utf8').replace(ANSI_RE, '') : '';
  };
  const robotText = readLog('robot.log');
  const serverText = readLog('server.log');

  const motion = {
    windows: 0,
    total_ticks: 0,
    total_missed: 0,
    total_skipped_slots: 0,
    missed_rate_pct: null,
    p95_cycle_us_max: 0,
    max_cycle_us: 0,
    max_work_us: 0,
    max_overrun_us: 0,
    neck_active: false,
  };
  const audioFirstPcmMs = [];
  const audioHeadersMs = [];
  let robotPanics = 0;
  let serverPanics = 0;
  let cameraErrors = 0;
  let neckDisarmedErrors = 0;
  let neckMotionStoppedErrors = 0;
  const neckStopReasons = [];
  let visionWindows = 0;
  let visionFramesMax = 0;
  let neuralWakeFallbackWarnings = 0;
  let neuralWakeLoaded = false;

  const kvNum = (line, key) => {
    const m = new RegExp(`\\b${key}=(\\d+)`).exec(line);
    return m ? Number(m[1]) : null;
  };
  const isPanic = (line) =>
    /panicked at|thread '.*' panicked|fatal runtime error|SIGSEGV|SIGABRT/i.test(line);

  for (const line of robotText.split('\n')) {
    if (isPanic(line)) robotPanics += 1;
    if (line.includes('neck disarmed')) neckDisarmedErrors += 1;
    if (
      line.includes('neck motion stopped') ||
      line.includes('neck worker panicked') ||
      line.includes('could not confirm neck torque off')
    ) {
      neckMotionStoppedErrors += 1;
      if (neckStopReasons.length < 10) {
        neckStopReasons.push(line.trim().slice(0, 260));
      }
    }
    if (line.includes('camera inference active')) {
      visionWindows += 1;
      const vf = kvNum(line, 'vision_frames');
      if (vf != null) visionFramesMax = Math.max(visionFramesMax, vf);
    }
    if (line.includes('camera stopped') || line.includes('camera error')) cameraErrors += 1;
    if (line.includes('local wake model unavailable; falling back')) {
      neuralWakeFallbackWarnings += 1;
    }
    if (line.includes('neural wake') || line.includes('onnx') || line.includes('wake model')) {
      neuralWakeLoaded = true;
    }
    if (line.includes('neck loop timing') || line.includes('neck loop missed 20ms deadline')) {
      motion.windows += 1;
      motion.neck_active = true;
      const totTicks = kvNum(line, 'total_ticks');
      const totMissed = kvNum(line, 'total_missed');
      const totSkipped = kvNum(line, 'total_skipped_slots');
      const winTicks = kvNum(line, 'ticks');
      const winMissed = kvNum(line, 'missed');
      const winSkipped = kvNum(line, 'skipped_slots');
      if (totTicks != null) {
        motion.total_ticks = Math.max(motion.total_ticks, totTicks);
      } else if (winTicks != null) {
        motion.total_ticks += winTicks;
      }
      if (totMissed != null) {
        motion.total_missed = Math.max(motion.total_missed, totMissed);
      } else if (winMissed != null) {
        motion.total_missed += winMissed;
      }
      if (totSkipped != null) {
        motion.total_skipped_slots = Math.max(motion.total_skipped_slots, totSkipped);
      } else if (winSkipped != null) {
        motion.total_skipped_slots += winSkipped;
      }
      const p95 = kvNum(line, 'p95_cycle_us');
      const maxC = kvNum(line, 'max_cycle_us');
      const maxW = kvNum(line, 'max_work_us');
      const maxO = kvNum(line, 'max_overrun_us');
      if (p95 != null) motion.p95_cycle_us_max = Math.max(motion.p95_cycle_us_max, p95);
      if (maxC != null) motion.max_cycle_us = Math.max(motion.max_cycle_us, maxC);
      if (maxW != null) motion.max_work_us = Math.max(motion.max_work_us, maxW);
      if (maxO != null) motion.max_overrun_us = Math.max(motion.max_overrun_us, maxO);
    }
    if (line.includes('speech first audio')) {
      const h = kvNum(line, 'headers_ms');
      const f = kvNum(line, 'first_pcm_ms');
      if (h != null) audioHeadersMs.push(h);
      if (f != null) audioFirstPcmMs.push(f);
    }
  }
  for (const line of serverText.split('\n')) {
    if (isPanic(line)) serverPanics += 1;
  }

  if (motion.total_ticks > 0) {
    motion.missed_rate_pct = Math.round((motion.total_missed / motion.total_ticks) * 10000) / 100;
  }

  return {
    motion_20ms: motion,
    audio_headers_ms: audioHeadersMs,
    audio_first_pcm_ms: audioFirstPcmMs,
    panics: {
      robot: robotPanics,
      server: serverPanics,
      total: robotPanics + serverPanics,
    },
    health: {
      camera_errors: cameraErrors,
      vision_windows: visionWindows,
      vision_frames_max: visionFramesMax,
      neck_disarmed_errors: neckDisarmedErrors,
      neck_motion_stopped_errors: neckMotionStoppedErrors,
      neck_stop_reasons: neckStopReasons,
      neural_wake_fallback_warnings: neuralWakeFallbackWarnings,
      neural_wake_log_seen: neuralWakeLoaded,
    },
  };
}

export function pidAlive(pid) {
  if (!pid) return false;
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

export function procCmdline(pid) {
  const p = `/proc/${pid}/cmdline`;
  if (fs.existsSync(p)) {
    try {
      return fs.readFileSync(p, 'utf8').replace(/\0+/g, ' ').trim();
    } catch {}
  }
  const r = spawnSync('ps', ['-o', 'command=', '-p', String(pid)], { encoding: 'utf8' });
  return (r.stdout || '').trim();
}

async function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.once('error', reject);
    s.listen(0, '127.0.0.1', () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
  });
}

async function freeEnginePort() {
  const probe = (port) =>
    new Promise((resolve) => {
      const s = net.createServer();
      s.once('error', () => resolve(false));
      s.listen(port, '127.0.0.1', () => s.close(() => resolve(true)));
    });
  for (let attempt = 0; attempt < 40; attempt++) {
    const base = 24000 + Math.floor(Math.random() * 15000);
    if ((await probe(base)) && (await probe(base + 1)) && (await probe(base + 10))) {
      return base;
    }
  }
  throw new Error('could not allocate free engine port triple (p, p+1, p+10)');
}

function findEnginePidByCmdline(serverPid) {
  for (const child of childPidsOf(serverPid)) {
    const cmd = procCmdline(child);
    if (cmd.includes('rivet-engine')) return child;
  }
  const r = spawnSync('pgrep', ['-f', 'rivet-engine'], { encoding: 'utf8' });
  if (r.status === 0) {
    for (const line of r.stdout.split('\n').filter(Boolean)) {
      const pid = Number(line.trim());
      if (Number.isFinite(pid) && pid > 0) {
        const statPath = `/proc/${pid}/stat`;
        if (fs.existsSync(statPath)) {
          const ppid = Number(fs.readFileSync(statPath, 'utf8').split(') ')[1]?.split(' ')[1]);
          if (ppid === serverPid) return pid;
        }
      }
    }
  }
  return null;
}

/** HTTP client for tests/pip-e2e/peripherals/supervisor.py control_url. */
export function createPeripheralClient(controlUrl) {
  const base = controlUrl.replace(/\/$/, '');
  const req = async (method, route, body = undefined) => {
    const res = await fetch(`${base}${route}`, {
      method,
      headers: body !== undefined ? { 'content-type': 'application/json' } : undefined,
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
    const text = await res.text();
    let parsed;
    try {
      parsed = JSON.parse(text);
    } catch {
      parsed = { raw: text };
    }
    if (!res.ok) {
      throw new Error(`peripherals ${method} ${route} HTTP ${res.status}: ${text.slice(0, 300)}`);
    }
    return parsed;
  };
  return {
    baseUrl: base,
    get: (route) => req('GET', route),
    post: (route, body = {}) => req('POST', route, body),
    health: () => req('GET', '/health'),
    status: () => req('GET', '/status'),
    faceEvents: (sinceSeq = 0, limit = 500) => req('GET', `/face/events?since_seq=${sinceSeq}&limit=${limit}`),
    faceHello: (opts = {}) => req('POST', '/face/hello', opts),
    faceAssertQrAfterHello: (opts) => req('POST', '/face/assert-qr-after-hello', opts),
    faceWait: (opts) => req('POST', '/face/wait', opts),
    faceClear: () => req('POST', '/face/clear', {}),
    servoState: () => req('GET', '/servo/state'),
    servoEvents: (kind = 'all', sinceSeq = 0, limit = 500) =>
      req('GET', `/servo/events?kind=${kind}&since_seq=${sinceSeq}&limit=${limit}`),
    servoWaitGoal: (opts) => req('POST', '/servo/wait-goal', opts),
    servoAssertBounds: (opts = {}) => req('POST', '/servo/assert-bounds', opts),
    servoClear: () => req('POST', '/servo/clear', {}),
    micInject: (opts) => req('POST', '/mic/inject', opts),
    micStatus: () => req('GET', '/mic/status'),
    micWaitIdle: (timeoutMs = 15000) => req('POST', '/mic/wait-idle', { timeout_ms: timeoutMs }),
    micNoise: (opts) => req('POST', '/mic/noise', opts),
    audioEcho: (opts) => req('POST', '/audio/echo', opts),
    speakerSessions: () => req('GET', '/speaker/sessions'),
    speakerWait: (opts) => req('POST', '/speaker/wait', opts),
    speakerClear: () => req('POST', '/speaker/clear', {}),
    cameraStatus: () => req('GET', '/camera/status'),
    cameraScene: (opts) => req('POST', '/camera/scene', opts),
    whisperStatus: () => req('GET', '/whisper/status'),
    whisperConfig: (opts) => req('POST', '/whisper/config', opts),
    whisperRelease: () => req('POST', '/whisper/release', {}),
    whisperWait: (opts) => req('POST', '/whisper/wait', opts),
    whisperClear: () => req('POST', '/whisper/clear', {}),
    nmcliState: (state) => req('POST', '/nmcli/state', { state }),
  };
}

/**
 * Starts the complete ARM64 Pip E2E stack (real server + pinned Rivet 3.1.1 +
 * TLS proxies + mock OIDC/model/speech + virtual Pip peripherals + real robot).
 */
export async function startPipE2EStack(options = {}) {
  const log = options.log ?? ((...a) => console.error('pip-e2e:', ...a));
  const serverBin = options.serverBin ?? process.env.PIP_E2E_SERVER_BIN ?? '/usr/local/bin/robot-buddy-server';
  const robotBin = options.robotBin ?? process.env.PIP_E2E_ROBOT_BIN ?? '/usr/local/bin/robot-buddy-robot';
  const engineBin = options.engineBin ?? process.env.BUDDY_ENGINE_BINARY ?? '/usr/local/bin/rivet-engine';
  const whisperBin =
    options.whisperBin ??
    process.env.PIP_E2E_WHISPER_BIN ??
    path.join(process.env.PIP_E2E_SPEECH_DIR ?? '/opt/pip-e2e/speech', 'bin/whisper-cli');
  const whisperModel =
    options.whisperModel ??
    process.env.PIP_E2E_WHISPER_MODEL ??
    path.join(process.env.PIP_E2E_CACHE_DIR ?? '/opt/pip-e2e/cache', 'speech/models/ggml-tiny.en.bin');
  const ortDylib =
    options.ortDylib ??
    process.env.ORT_DYLIB_PATH ??
    path.join(process.env.PIP_E2E_SPEECH_DIR ?? '/opt/pip-e2e/speech', 'lib/libonnxruntime.so');
  const wakeCacheDir =
    options.wakeCacheDir ??
    path.join(process.env.PIP_E2E_CACHE_DIR ?? '/opt/pip-e2e/cache', 'speech/models/wake');
  const privateHeyPipOnnx =
    options.privateHeyPipOnnx ??
    process.env.PIP_E2E_HEY_PIP_ONNX ??
    '/opt/pip-e2e/private/hey_pip.onnx';
  const visionCacheDir =
    options.visionCacheDir ??
    process.env.PIP_E2E_VISION_CACHE ??
    path.join(process.env.PIP_E2E_CACHE_DIR ?? '/opt/pip-e2e/cache', 'vision');

  for (const [name, bin] of [
    ['robot-buddy-server', serverBin],
    ['robot-buddy-robot', robotBin],
    ['rivet-engine', engineBin],
  ]) {
    if (!fs.existsSync(bin)) {
      throw new Error(`required binary missing: ${name} at ${bin}`);
    }
  }

  const dataRoot = process.env.PIP_E2E_DATA_ROOT && fs.existsSync(process.env.PIP_E2E_DATA_ROOT)
    ? process.env.PIP_E2E_DATA_ROOT
    : '/tmp';
  const work = fs.mkdtempSync(path.join(dataRoot, 'pip-e2e-'));
  const dataDir = path.join(work, 'data');
  const periphDir = path.join(work, 'peripherals');
  const galleryDir = path.join(work, 'faces');
  const wakeRunDir = path.join(work, 'wake-models');
  fs.mkdirSync(dataDir, { recursive: true, mode: 0o700 });
  fs.mkdirSync(periphDir, { recursive: true, mode: 0o755 });
  fs.mkdirSync(galleryDir, { recursive: true, mode: 0o700 });

  const tlsBundle = createTestTls(work);
  const enginePort = await freeEnginePort();
  const serverPort = await freePort();
  const screenPort = await freePort();
  const operatorPort = await freePort();
  const operatorToken = crypto.randomBytes(24).toString('hex');

  // Secrets held ONLY in memory for the artifact redaction gate.
  const redactionSecrets = new Set([operatorToken]);

  const model = startMockModel({ script: options.modelScript ?? null });
  const speech = startMockSpeech({
    firstByteMs: options.speechFirstByteMs ?? 60,
    msPerChar: options.speechMsPerChar ?? 30,
    realtime: options.speechRealtime ?? true,
  });
  const oidc = startMockOidc({ key: tlsBundle.key, cert: tlsBundle.cert });
  redactionSecrets.add(oidc.clientSecret);
  redactionSecrets.add(speech.apiKey);

  const serverProxy = await startTlsFaultProxy({
    upstreamPort: serverPort,
    key: tlsBundle.key,
    cert: tlsBundle.cert,
    name: 'server-tls',
  });
  const rivetProxy = await startTlsFaultProxy({
    upstreamPort: enginePort,
    key: tlsBundle.key,
    cert: tlsBundle.cert,
    name: 'rivet-tls',
  });

  const secretsDir = path.join(work, 'secrets');
  const identityFile = path.join(work, 'device.identity.json');
  const cacheFile = path.join(work, 'robot-cache.json');
  const runCmd = (cmd, args) => {
    const r = spawnSync(cmd, args, { encoding: 'utf8' });
    if (r.status !== 0) throw new Error(`${path.basename(cmd)} ${args.join(' ')} failed: ${r.stderr}`);
  };
  runCmd(serverBin, ['init', secretsDir]);
  runCmd(serverBin, ['provision', path.join(secretsDir, 'issuer.key'), identityFile]);
  fs.chmodSync(identityFile, 0o600);

  const secrets = Object.fromEntries(
    fs
      .readFileSync(path.join(secretsDir, 'secrets.env'), 'utf8')
      .split('\n')
      .filter(Boolean)
      .map((l) => [l.slice(0, l.indexOf('=')), l.slice(l.indexOf('=') + 1)]),
  );
  for (const v of Object.values(secrets)) {
    if (v && v.length >= 12) redactionSecrets.add(v);
  }
  const identity = JSON.parse(fs.readFileSync(identityFile, 'utf8'));
  if (identity.private_key) redactionSecrets.add(identity.private_key);

  const engineUrl = `http://127.0.0.1:${enginePort}`;
  const serverDirectUrl = `http://127.0.0.1:${serverPort}`;

  const leaks = [];
  let serverProc = null;
  let enginePid = null;
  let periphProc = null;
  let periphReady = null;
  let periph = null;
  let robotProc = null;
  let robotLogFd = null;
  let serverLogFd = null;
  let periphLogFd = null;
  let stopped = false;

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
  const waitEvent = (match, ms = 30_000, what = 'event') => {
    const existing = events.find(match);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const w = { match, resolve };
      waiters.add(w);
      setTimeout(() => {
        if (waiters.delete(w)) reject(new Error(`timed out waiting for ${what} (${ms} ms)`));
      }, ms);
    });
  };
  const until = async (what, fn, ms = 60_000) => {
    const deadline = Date.now() + ms;
    let lastErr;
    while (Date.now() < deadline) {
      try {
        const v = await fn();
        if (v) return v;
      } catch (e) {
        lastErr = e;
      }
      await sleep(75);
    }
    throw new Error(`timed out waiting for ${what}${lastErr ? `: ${lastErr.message}` : ''}`);
  };

  const tlsFetch = (url, init = {}) =>
    fetch(url, {
      ...init,
      tls: { ca: tlsBundle.ca },
    });

  const makeAdminSession = (botId) => {
    const body = b64u(JSON.stringify({ bot_id: botId, role: 'admin', expires: Math.floor(Date.now() / 1000) + 3600 }));
    const mac = crypto.createHmac('sha256', secrets.BUDDY_SIGNING_KEY).update(`session.${body}`).digest();
    return `${body}.${b64u(mac)}`;
  };

  let enrolledBundle = null;
  const enrollDeviceDirect = async () => {
    const PKCS8_ED25519 = Buffer.from('302e020100300506032b657004220420', 'hex');
    const deviceKey = crypto.createPrivateKey({
      key: Buffer.concat([PKCS8_ED25519, Buffer.from(identity.private_key, 'base64url')]),
      format: 'der',
      type: 'pkcs8',
    });
    const chRes = await tlsFetch(`${serverProxy.url}/v1/challenge`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(identity.certificate),
    });
    if (!chRes.ok) throw new Error(`/v1/challenge HTTP ${chRes.status}`);
    const { challenge } = await chRes.json();
    const proof = b64u(crypto.sign(null, Buffer.from(challenge), deviceKey));
    const enRes = await tlsFetch(`${serverProxy.url}/v1/enroll`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ certificate: identity.certificate, challenge, proof }),
    });
    if (!enRes.ok) throw new Error(`/v1/enroll HTTP ${enRes.status}`);
    enrolledBundle = await enRes.json();
    if (enrolledBundle.session) redactionSecrets.add(enrolledBundle.session);
    if (enrolledBundle.gateway_token) redactionSecrets.add(enrolledBundle.gateway_token);
    return enrolledBundle;
  };

  const actorCall = async (actorId, token, command) => {
    if (!enrolledBundle) await enrollDeviceDirect();
    const r = await fetch(
      `${engineUrl}/gateway/${encodeURIComponent(actorId)}@${encodeURIComponent(enrolledBundle.gateway_token)}/action/call`,
      {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          'x-rivet-encoding': 'json',
          'x-rivet-conn-params': JSON.stringify({ session: token }),
          'x-rivet-target': 'actor',
          'x-rivet-actor': actorId,
          'x-rivet-token': enrolledBundle.gateway_token,
          'x-rivet-namespace': enrolledBundle.namespace,
        },
        body: JSON.stringify({ args: [{ session: token, command }] }),
      },
    );
    if (!r.ok) throw new Error(`actor ${command.type} HTTP ${r.status}: ${await r.text()}`);
    return (await r.json()).output;
  };

  const botCall = async (command, role = 'admin') => {
    if (!enrolledBundle) await enrollDeviceDirect();
    const token = role === 'admin' ? makeAdminSession(enrolledBundle.bot_id) : enrolledBundle.session;
    return actorCall(enrolledBundle.actor_id, token, command);
  };

  const conversationCall = async (command, role = 'admin') => {
    if (!enrolledBundle) await enrollDeviceDirect();
    const token = role === 'admin' ? makeAdminSession(enrolledBundle.bot_id) : enrolledBundle.session;
    return actorCall(enrolledBundle.conversation_actor_id, token, command);
  };

  const stopProcess = async (proc, name, ms = 15_000) => {
    if (!proc || proc.exitCode !== null || proc.signalCode !== null) return null;
    const t0 = now();
    proc.kill('SIGTERM');
    const deadline = Date.now() + ms;
    while (proc.exitCode === null && proc.signalCode === null && Date.now() < deadline) {
      await sleep(50);
    }
    if (proc.exitCode === null && proc.signalCode === null) {
      leaks.push(`${name} ignored SIGTERM for ${ms} ms`);
      proc.kill('SIGKILL');
      return null;
    }
    return Math.round(now() - t0);
  };

  const startServerProcess = async () => {
    serverLogFd = fs.openSync(path.join(work, 'server.log'), 'a');
    const env = {
      PATH: process.env.PATH,
      HOME: work,
      TMPDIR: work,
      SSL_CERT_FILE: tlsBundle.bundlePath,
      RUST_LOG: options.serverLog ?? process.env.PIP_E2E_SERVER_LOG ?? 'info',
      ...secrets,
      RIVET_ENDPOINT: engineUrl,
      RIVETKIT_ENGINE_SPAWN: 'never',
      RIVET_NAMESPACE: 'default',
      RIVET_POOL: 'default',
      BUDDY_ENGINE_BINARY: engineBin,
      BUDDY_ENGINE_PORT: String(enginePort),
      BUDDY_DATA_DIR: dataDir,
      BUDDY_LISTEN: `127.0.0.1:${serverPort}`,
      BUDDY_PUBLIC_URL: serverProxy.url,
      BUDDY_PUBLIC_RIVET_URL: rivetProxy.url,
      OIDC_ISSUER: oidc.issuer,
      OIDC_CLIENT_ID: oidc.clientId,
      OIDC_CLIENT_SECRET: oidc.clientSecret,
      AI_PROVIDER: 'openai_compatible',
      AI_BASE_URL: model.baseUrl,
      AI_MODEL: 'pip-e2e-fixture',
      TTS_MODEL: 'elevenlabs/eleven-v4-turbo',
      TTS_BASE_URL: speech.baseUrl,
      TTS_API_KEY: speech.apiKey,
      NO_PROXY: '127.0.0.1,localhost',
      no_proxy: '127.0.0.1,localhost',
    };
    serverProc = spawn(serverBin, [], {
      env,
      stdio: ['ignore', serverLogFd, serverLogFd],
    });
    await until('server /ready', async () => {
      if (serverProc.exitCode !== null) throw new Error(`server exited ${serverProc.exitCode}`);
      const r = await fetch(`${serverDirectUrl}/ready`);
      return r.ok;
    }, 45_000);
    enginePid = await until('rivet-engine child PID', async () => findEnginePidByCmdline(serverProc.pid), 15_000);
    log(`server pid=${serverProc.pid} ready with rivet-engine pid=${enginePid}`);
    return { serverPid: serverProc.pid, enginePid };
  };

  const stopServerAndEngine = async () => {
    const prevServerPid = serverProc?.pid ?? null;
    const prevEnginePid = enginePid;
    const exitMs = await stopProcess(serverProc, 'robot-buddy-server', 35_000);
    if (prevEnginePid) {
      const deadline = Date.now() + 15_000;
      while (pidAlive(prevEnginePid) && Date.now() < deadline) await sleep(100);
      if (pidAlive(prevEnginePid)) {
        leaks.push(`rivet-engine pid ${prevEnginePid} outlived server stop`);
        try {
          process.kill(prevEnginePid, 'SIGKILL');
        } catch {}
      }
    }
    serverProc = null;
    enginePid = null;
    return { prevServerPid, prevEnginePid, exitMs };
  };

  const startPeripherals = async () => {
    const supervisorPath = path.join(root, 'tests/pip-e2e/peripherals/supervisor.py');
    if (!fs.existsSync(supervisorPath)) {
      throw new Error(`peripherals supervisor missing at ${supervisorPath}`);
    }
    const readyFile = path.join(periphDir, 'peripherals.json');
    periphLogFd = fs.openSync(path.join(work, 'peripherals.log'), 'a');
    const args = [
      supervisorPath,
      '--state-dir',
      periphDir,
      '--control-port',
      '0',
      '--with-fake-nmcli',
    ];
    if (fs.existsSync(whisperBin)) {
      args.push('--real-whisper', whisperBin);
    }
    periphProc = spawn('python3', args, {
      env: { ...process.env, PYTHONUNBUFFERED: '1' },
      stdio: ['ignore', 'pipe', periphLogFd],
    });
    await until('peripherals ready.json', async () => {
      if (periphProc.exitCode !== null) throw new Error(`peripherals exited ${periphProc.exitCode}`);
      return fs.existsSync(readyFile) ? JSON.parse(fs.readFileSync(readyFile, 'utf8')) : null;
    }, 20_000);
    periphReady = JSON.parse(fs.readFileSync(readyFile, 'utf8'));
    periph = createPeripheralClient(periphReady.control_url);
    await until('peripherals /health', async () => (await periph.health()).ok, 10_000);
    log(`peripherals pid=${periphProc.pid} control=${periphReady.control_url}`);
    return periphReady;
  };

  const canRunNeuralWake = () => {
    const mel = path.join(wakeCacheDir, 'melspectrogram.onnx');
    const emb = path.join(wakeCacheDir, 'embedding_model.onnx');
    return fs.existsSync(ortDylib) && fs.existsSync(mel) && fs.existsSync(emb) && fs.existsSync(privateHeyPipOnnx);
  };

  const prepareWakeDir = () => {
    const mel = path.join(wakeCacheDir, 'melspectrogram.onnx');
    const emb = path.join(wakeCacheDir, 'embedding_model.onnx');
    if (!canRunNeuralWake()) return null;
    fs.mkdirSync(wakeRunDir, { recursive: true, mode: 0o755 });
    for (const [name, target] of [
      ['melspectrogram.onnx', mel],
      ['embedding_model.onnx', emb],
      ['hey_pip.onnx', privateHeyPipOnnx],
    ]) {
      const link = path.join(wakeRunDir, name);
      if (fs.existsSync(link)) fs.rmSync(link, { force: true });
      fs.symlinkSync(target, link);
    }
    return wakeRunDir;
  };

  const yunetPath = path.join(visionCacheDir, 'face_detection_yunet_2023mar.onnx');
  const sfacePath = path.join(visionCacheDir, 'face_recognition_sface_2021dec.onnx');

  const startRobotProcess = async (robotOpts = {}) => {
    const enableVision = robotOpts.vision ?? (options.vision !== false && fs.existsSync(yunetPath));
    const enableNeuralWake = robotOpts.neuralWake ?? false;
    const wakeDir = enableNeuralWake ? prepareWakeDir() : null;
    if (enableNeuralWake && !wakeDir) {
      throw new Error('neural wake requested but ORT_DYLIB_PATH / openWakeWord / hey_pip.onnx missing');
    }
    if (!fs.existsSync(whisperModel)) {
      throw new Error(`whisper model missing at ${whisperModel}`);
    }
    if (!fs.existsSync(whisperBin)) {
      throw new Error(`whisper-cli binary missing at ${whisperBin}`);
    }

    const robotEnv = {
      PATH: `${periphReady.bin_dir}:${path.dirname(whisperBin)}:${process.env.PATH}`,
      HOME: work,
      TMPDIR: work,
      SSL_CERT_FILE: tlsBundle.bundlePath,
      // Capture motion 50 Hz periodic timing (info) and speech first-PCM latency (debug).
      RUST_LOG:
        robotOpts.logLevel ??
        process.env.PIP_E2E_ROBOT_LOG ??
        'info,robot_buddy_robot::motion=info,robot_buddy_robot::audio=debug',
      BUDDY_SERVER_URL: serverProxy.url,
      BUDDY_IDENTITY_FILE: identityFile,
      BUDDY_CACHE_FILE: cacheFile,
      BUDDY_EVENTS: 'compact',
      BUDDY_SPEECH: 'true',
      BUDDY_ALSA_DEVICE: periphReady.env?.BUDDY_ALSA_DEVICE ?? 'virtual-speaker',
      BUDDY_SCREEN_ADDR: `127.0.0.1:${screenPort}`,
      BUDDY_OPERATOR_ADDR: `127.0.0.1:${operatorPort}`,
      BUDDY_OPERATOR_TOKEN: operatorToken,
      BUDDY_FACE_SERIAL: periphReady.face_serial,
      BUDDY_SERVO_PORT: periphReady.servo_port,
      BUDDY_SERVO_CALIBRATION: periphReady.servo_calibration,
      BUDDY_LISTEN_MODEL: whisperModel,
      BUDDY_LISTEN_WHISPER: robotOpts.useSlowWhisperWrapper !== false ? periphReady.whisper_bin : whisperBin,
      BUDDY_LISTEN_DEVICE: periphReady.env?.BUDDY_LISTEN_DEVICE ?? 'virtual-mic',
      BUDDY_LISTEN_CHANNELS: '1',
      BUDDY_LISTEN_CHANNEL: '0',
      BUDDY_LISTEN_BARGE_IN: enableNeuralWake ? '1' : '0',
      BUDDY_WIFI_GRACE_SECS: '3600',
      NO_PROXY: '127.0.0.1,localhost',
      no_proxy: '127.0.0.1,localhost',
    };
    if (wakeDir) {
      robotEnv.ORT_DYLIB_PATH = ortDylib;
      robotEnv.BUDDY_LISTEN_WAKE_DIR = wakeDir;
      if (robotOpts.wakeThreshold != null) {
        robotEnv.BUDDY_LISTEN_WAKE_THRESHOLD = String(robotOpts.wakeThreshold);
      }
    }
    if (enableVision) {
      robotEnv.BUDDY_VISION = '1';
      robotEnv.BUDDY_VISION_CAMERA = periphReady.camera_bin;
      robotEnv.BUDDY_VISION_YUNET = yunetPath;
      if (fs.existsSync(sfacePath)) {
        robotEnv.BUDDY_VISION_SFACE = sfacePath;
      }
      robotEnv.BUDDY_VISION_GALLERY = galleryDir;
    }

    robotLogFd = fs.openSync(path.join(work, 'robot.log'), 'a');
    // NO --simulate flag! Real hardware service mode over real TLS.
    // Note: tracing_subscriber::fmt writes to stdout alongside compact JSON event lines,
    // so tee every stdout line into robot.log while parsing compact JSON events.
    robotProc = spawn(robotBin, [], {
      env: robotEnv,
      stdio: ['pipe', 'pipe', robotLogFd],
    });
    readline.createInterface({ input: robotProc.stdout }).on('line', (line) => {
      const t = now();
      try {
        fs.writeSync(robotLogFd, `${line}\n`);
      } catch {}
      const clean = line.replace(ANSI_RE, '').trim();
      if (clean.startsWith('{')) {
        try {
          const e = JSON.parse(clean);
          if (e.kind && e.turn_id) onEvent({ ...e, t });
        } catch {}
        return;
      }
      // Compact hardware-mode journal line: `event sequence=1 turn=<uuid> kind=turn.started`
      const m = /^event\s+sequence=(\d+)\s+turn=(\S+)\s+kind=(\S+)$/.exec(clean);
      if (m) {
        onEvent({ sequence: Number(m[1]), turn_id: m[2], kind: m[3], t });
      }
    });

    await until('robot panel HTTP listening', async () => {
      if (robotProc.exitCode !== null) throw new Error(`robot exited ${robotProc.exitCode}`);
      const r = await fetch(`http://127.0.0.1:${screenPort}/`);
      return r.ok;
    }, 30_000);
    log(`robot pid=${robotProc.pid} started (hardware mode, vision=${enableVision}, neuralWake=${Boolean(wakeDir)})`);
    return {
      pid: robotProc.pid,
      vision: enableVision,
      neuralWake: Boolean(wakeDir),
    };
  };

  const stopRobotProcess = async (ms = 10_000) => {
    if (!robotProc) return null;
    const pid = robotProc.pid;
    const exitMs = await stopProcess(robotProc, 'robot-buddy-robot', ms);
    const exitCode = robotProc.exitCode;
    const signalCode = robotProc.signalCode;
    robotProc = null;
    await sleep(200);
    const remainingChildren = descendantPidsOf(pid).filter(pidAlive);
    return { pid, exitMs, exitCode, signalCode, remainingChildren, cacheExists: fs.existsSync(cacheFile) };
  };

  /**
   * Executes the full real production enrollment + OIDC claim + loopback panel confirmation.
   * Returns REDACTED audit metadata only (never claim URLs, tokens, or 6-digit codes).
   */
  const completeOidcClaimViaPanel = async () => {
    const authHeader = `Basic ${Buffer.from(`operator:${operatorToken}`).toString('base64')}`;
    const unauth = await fetch(`http://127.0.0.1:${operatorPort}/`);
    const unclaimed = await fetch(`http://127.0.0.1:${operatorPort}/`, {
      headers: { authorization: authHeader },
    });

    await until('robot shows claim QR', async () => {
      const html = await (await fetch(`http://127.0.0.1:${screenPort}/`)).text();
      return html.includes('Scan to register your robot');
    }, 30_000);

    const claimUrl = await until('robot-buddy-robot claim-url', async () => {
      const r = spawnSync(robotBin, ['claim-url'], {
        env: { PATH: process.env.PATH, BUDDY_CACHE_FILE: cacheFile },
        encoding: 'utf8',
      });
      const out = (r.stdout || '').trim();
      return r.status === 0 && out.startsWith('https://') ? out : null;
    }, 20_000);

    const parsedClaimUrl = new URL(claimUrl);
    const claimTokenParam = parsedClaimUrl.searchParams.get('token');
    if (claimTokenParam) redactionSecrets.add(claimTokenParam);
    redactionSecrets.add(claimUrl);

    const claimRes = await tlsFetch(claimUrl);
    if (!claimRes.ok) throw new Error(`GET /claim HTTP ${claimRes.status}`);
    const claimHtml = await claimRes.text();
    const claimCookie = (claimRes.headers.get('set-cookie') ?? '').split(';')[0];
    const field = (html, name) => {
      const m = new RegExp(`name=${name}\\s+value=["']([^"']+)["']`).exec(html);
      if (!m) throw new Error(`missing form field ${name}`);
      return m[1];
    };
    const bot = field(claimHtml, 'bot');
    const token = field(claimHtml, 'token');
    const csrf = field(claimHtml, 'csrf');
    redactionSecrets.add(token);

    const loginRes = await tlsFetch(`${serverProxy.url}/login`, {
      method: 'POST',
      headers: {
        'content-type': 'application/x-www-form-urlencoded',
        origin: serverProxy.url,
        cookie: claimCookie,
      },
      body: new URLSearchParams({ bot, token, csrf }).toString(),
      redirect: 'manual',
    });
    if (loginRes.status !== 302 && loginRes.status !== 303) {
      throw new Error(`POST /login HTTP ${loginRes.status}`);
    }
    const authorizeUrl = loginRes.headers.get('location');
    const loginCookies = loginRes.headers.getSetCookie?.() ?? [loginRes.headers.get('set-cookie') ?? ''];
    const loginCookie = loginCookies.map((c) => c.split(';')[0]).find((c) => c.startsWith('buddy_login='));
    if (!authorizeUrl || !loginCookie) {
      throw new Error('POST /login did not return authorize redirect and buddy_login cookie');
    }

    const authRes = await tlsFetch(authorizeUrl, { redirect: 'manual' });
    if (authRes.status !== 302) throw new Error(`GET /authorize HTTP ${authRes.status}`);
    const callbackUrl = authRes.headers.get('location');
    if (!callbackUrl) throw new Error('OIDC /authorize missing Location header');

    const cbRes = await tlsFetch(callbackUrl, {
      headers: { cookie: loginCookie },
    });
    if (!cbRes.ok) throw new Error(`GET /auth/callback HTTP ${cbRes.status}`);
    const cbHtml = await cbRes.text();
    const codeMatch = /\b(\d{6})\b/.exec(cbHtml);
    if (!codeMatch) throw new Error(`6-digit confirm code not found in callback HTML`);
    const confirmCode = codeMatch[1];
    // Track confirmation code so the redaction gate proves it never leaks into artifacts.
    redactionSecrets.add(confirmCode);

    let panelCsrf = null;
    await until('panel shows confirm code', async () => {
      const html = await (await fetch(`http://127.0.0.1:${screenPort}/`)).text();
      if (!html.includes(confirmCode)) return false;
      const m = /name=csrf\s+value="([^"]+)"/.exec(html);
      if (!m) return false;
      panelCsrf = m[1];
      return true;
    }, 20_000);

    const confirmRes = await fetch(`http://127.0.0.1:${screenPort}/confirm`, {
      method: 'POST',
      headers: {
        host: `127.0.0.1:${screenPort}`,
        origin: `http://127.0.0.1:${screenPort}`,
        'content-type': 'application/x-www-form-urlencoded',
      },
      body: new URLSearchParams({ csrf: panelCsrf }).toString(),
      redirect: 'manual',
    });
    if (confirmRes.status !== 302 && confirmRes.status !== 303 && confirmRes.status !== 200) {
      throw new Error(`POST /confirm HTTP ${confirmRes.status}`);
    }

    await until('robot panel Ready', async () => {
      const html = await (await fetch(`http://127.0.0.1:${screenPort}/`)).text();
      return html.includes('Ready');
    }, 25_000);

    const claimed = await fetch(`http://127.0.0.1:${operatorPort}/`, {
      headers: { authorization: authHeader },
    });
    const operatorHtml = await claimed.text();
    const snap = await botCall({ type: 'snapshot' });

    return {
      unauth_status: unauth.status,
      unclaimed_status: unclaimed.status,
      claimed_status: claimed.status,
      operator_ready: operatorHtml.includes('Robot operator'),
      claim_url_https_verified: parsedClaimUrl.protocol === 'https:',
      confirm_code_matched: true,
      owner_matched: snap.owner === oidc.expectedOwner,
      oidc_audit: { ...oidc.audit },
    };
  };

  /**
   * Restarts BOTH real robot-buddy-server and real rivet-engine 3.1.1 against the
   * SAME persistent dataDir.
   */
  const restartServerAndEngine = async ({ killEngineFirst = false } = {}) => {
    const beforeServerPid = serverProc?.pid ?? null;
    const beforeEnginePid = enginePid;
    let engineKilledExitStatus = null;

    if (killEngineFirst && beforeEnginePid && pidAlive(beforeEnginePid)) {
      log(`fault: killing rivet-engine pid=${beforeEnginePid} (SIGKILL)`);
      process.kill(beforeEnginePid, 'SIGKILL');
      await until('rivet-engine killed', async () => !pidAlive(beforeEnginePid), 10_000);
      await sleep(500);
      if (serverProc && serverProc.exitCode !== null) {
        engineKilledExitStatus = serverProc.exitCode;
      }
    }

    await stopServerAndEngine();
    const persistedLogs = fs.existsSync(path.join(dataDir, 'logs'));
    const persistedObjects = fs.existsSync(path.join(dataDir, 'objects'));
    const restarted = await startServerProcess();
    enrolledBundle = null;
    await enrollDeviceDirect();
    return {
      before_server_pid: beforeServerPid,
      before_engine_pid: beforeEnginePid,
      after_server_pid: restarted.serverPid,
      after_engine_pid: restarted.enginePid,
      server_pid_changed: restarted.serverPid !== beforeServerPid,
      engine_pid_changed: restarted.enginePid !== beforeEnginePid,
      engine_killed_exit_status: engineKilledExitStatus,
      persisted_logs: persistedLogs,
      persisted_objects: persistedObjects,
    };
  };

  const sampleMetrics = () => {
    const robot = readProcMetrics(robotProc?.pid);
    const children = readDescendantMetrics(robotProc?.pid);
    const server = readProcMetrics(serverProc?.pid);
    const engine = readProcMetrics(enginePid);
    const cgroup = readCgroupMemory();
    return {
      t: Math.round(now()),
      robot,
      robot_children: children,
      robot_tree_rss_kib:
        robot?.rss_kib != null ? robot.rss_kib + (children?.rss_kib ?? 0) : null,
      cgroup_memory: cgroup,
      server,
      engine,
    };
  };

  const operatorRequest = async (route, { method = 'GET', form = null } = {}) => {
    const authHeader = `Basic ${Buffer.from(`operator:${operatorToken}`).toString('base64')}`;
    const headers = {
      authorization: authHeader,
      host: `127.0.0.1:${operatorPort}`,
    };
    let body;
    if (method === 'POST') {
      headers.origin = `http://127.0.0.1:${operatorPort}`;
      headers['content-type'] = 'application/x-www-form-urlencoded';
      body = new URLSearchParams(form ?? {}).toString();
    }
    return fetch(`http://127.0.0.1:${operatorPort}${route}`, {
      method,
      headers,
      body,
      redirect: 'manual',
    });
  };

  /**
   * Redaction gate: fails if any claim URL, claim token, 6-digit confirm code,
   * operator token, OIDC secret, signing key, Rivet token, device private key,
   * or gallery embedding pattern appears in an output artifact string.
   */
  const checkRedaction = (texts) => {
    const findings = [];
    const combined = texts.join('\n');
    for (const secret of redactionSecrets) {
      if (!secret || secret.length < 6) continue;
      // For 6-digit confirm codes, match word-bounded to avoid matching unrelated timestamps/hashes.
      if (/^\d{6}$/.test(secret)) {
        if (new RegExp(`(?<![0-9a-fA-F])${secret}(?![0-9a-fA-F])`).test(combined)) {
          findings.push('6-digit confirmation code leaked in artifact');
        }
      } else if (combined.includes(secret)) {
        findings.push(`secret/credential value (len=${secret.length}) leaked in artifact`);
      }
    }
    if (/\/claim\?bot=[^\s"']*&token=/i.test(combined)) {
      findings.push('claim URL with token query parameter leaked in artifact');
    }
    if (/BEGIN (EC |RSA |)PRIVATE KEY/i.test(combined)) {
      findings.push('PEM private key leaked in artifact');
    }
    return { ok: findings.length === 0, findings };
  };

  const shutdown = async () => {
    if (stopped) return;
    stopped = true;
    const robotStop = await stopRobotProcess(10_000);
    await stopServerAndEngine();
    await stopProcess(periphProc, 'peripherals-supervisor', 10_000);
    serverProxy.close();
    rivetProxy.close();
    model.stop();
    speech.stop();
    oidc.stop();
    for (const fd of [robotLogFd, serverLogFd, periphLogFd]) {
      if (fd != null) {
        try {
          fs.closeSync(fd);
        } catch {}
      }
    }
    if (!options.keep) {
      fs.rmSync(work, { recursive: true, force: true });
    }
    return { robotStop, leaks };
  };

  const tails = (lines = 40) =>
    ['server.log', 'robot.log', 'peripherals.log']
      .map((f) => path.join(work, f))
      .filter((p) => fs.existsSync(p))
      .map((p) => `--- ${path.basename(p)} (last ${lines} lines)\n${fs.readFileSync(p, 'utf8').split('\n').slice(-lines).join('\n')}`)
      .join('\n');

  const checkNeuralWakeMapped = () => {
    const pid = robotProc?.pid;
    if (!pid) return { mapped: false, fallback_warnings: null };
    let mapped = false;
    const mapsPath = `/proc/${pid}/maps`;
    if (fs.existsSync(mapsPath)) {
      try {
        mapped = fs.readFileSync(mapsPath, 'utf8').includes('libonnxruntime.so');
      } catch {}
    }
    const tel = parseLogTelemetry(work);
    return {
      mapped,
      fallback_warnings: tel.health.neural_wake_fallback_warnings,
    };
  };

  try {
    await startServerProcess();
    await startPeripherals();
    if (options.startRobot !== false) {
      await startRobotProcess({ vision: options.vision, neuralWake: options.neuralWake });
    }
  } catch (e) {
    e.logs = tails(50);
    await shutdown();
    throw e;
  }

  return {
    work,
    dataDir,
    cacheFile,
    identityFile,
    ports: { enginePort, serverPort, screenPort, operatorPort },
    tlsBundle,
    serverProxy,
    rivetProxy,
    model,
    speech,
    oidc,
    periph,
    periphReady,
    events,
    leaks,
    paths: {
      serverBin,
      robotBin,
      engineBin,
      whisperBin,
      whisperModel,
      ortDylib: fs.existsSync(ortDylib) ? ortDylib : null,
      melModel: fs.existsSync(path.join(wakeCacheDir, 'melspectrogram.onnx'))
        ? path.join(wakeCacheDir, 'melspectrogram.onnx')
        : null,
      embModel: fs.existsSync(path.join(wakeCacheDir, 'embedding_model.onnx'))
        ? path.join(wakeCacheDir, 'embedding_model.onnx')
        : null,
      heyPipModel: fs.existsSync(privateHeyPipOnnx) ? privateHeyPipOnnx : null,
      yunetModel: fs.existsSync(yunetPath) ? yunetPath : null,
      sfaceModel: fs.existsSync(sfacePath) ? sfacePath : null,
    },
    get serverPid() {
      return serverProc?.pid ?? null;
    },
    get enginePid() {
      return enginePid;
    },
    get robotPid() {
      return robotProc?.pid ?? null;
    },
    canRunNeuralWake,
    checkNeuralWakeMapped,
    waitEvent,
    until,
    botCall,
    conversationCall,
    operatorRequest,
    completeOidcClaimViaPanel,
    startRobotProcess,
    stopRobotProcess,
    restartServerAndEngine,
    sampleMetrics,
    parseLogTelemetry: () => parseLogTelemetry(work),
    checkRedaction,
    tails,
    shutdown,
  };
}
