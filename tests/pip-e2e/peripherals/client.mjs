// Zero-dependency Node.js / Bun ESM client for tests/pip-e2e/peripherals/daemon.py.
// Supports both Unix domain socket newline-delimited JSON-RPC (<run_dir>/control.sock)
// and optional HTTP fallback via control_url.

import net from 'node:net';
import fs from 'node:fs/promises';
import path from 'node:path';
import { spawn } from 'node:child_process';

/**
 * Connect to a running peripheral supervisor over its Unix JSON-RPC socket.
 * @param {string} sockPath
 * @returns {Promise<PeripheralsClient>}
 */
export async function connectPeripherals(sockPath) {
  const socket = net.createConnection({ path: sockPath });
  await new Promise((resolve, reject) => {
    socket.once('connect', resolve);
    socket.once('error', reject);
  });
  return new PeripheralsClient(socket, sockPath);
}

/**
 * Wait until <readyFile> exists and is valid JSON, then return its parsed manifest.
 * @param {string} readyFile
 * @param {number} [timeoutMs=10000]
 */
export async function waitForReadyFile(readyFile, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const text = await fs.readFile(readyFile, 'utf8');
      return JSON.parse(text);
    } catch {
      await new Promise((r) => setTimeout(r, 50));
    }
  }
  throw new Error(`Timed out waiting for peripheral manifest: ${readyFile}`);
}

/**
 * Convenience launcher for tests/pip-e2e/runner*.mjs: spawns daemon.py, waits for
 * ready.json, connects client, and returns { manifest, client, stop }.
 */
export async function startPeripherals(options) {
  const {
    runDir,
    realWhisper = '/usr/local/bin/whisper-cli',
    calibration = null,
    controlPort = 0,
    withFakeNmcli = false,
    unprovisionedServos = false,
    captureDir = null,
    pythonBin = 'python3',
    timeoutMs = 15000,
  } = options;

  const daemonPath = path.resolve(
    path.dirname(new URL(import.meta.url).pathname),
    'daemon.py',
  );
  const readyFile = path.join(runDir, 'ready.json');
  const args = [
    daemonPath,
    '--run-dir',
    runDir,
    '--control-port',
    String(controlPort),
    '--real-whisper',
    realWhisper,
    '--ready-file',
    readyFile,
  ];
  if (calibration) args.push('--calibration', calibration);
  if (withFakeNmcli) args.push('--with-fake-nmcli');
  if (unprovisionedServos) args.push('--unprovisioned-servos');
  if (captureDir) args.push('--capture-dir', captureDir);

  let stderrBuf = '';
  const child = spawn(pythonBin, args, {
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  child.stderr.on('data', (d) => {
    stderrBuf += d.toString();
    if (stderrBuf.length > 8192) stderrBuf = stderrBuf.slice(-8192);
  });

  const exitedEarly = new Promise((_, reject) => {
    child.once('exit', (code, sig) => {
      reject(
        new Error(
          `peripherals daemon exited early (code=${code}, sig=${sig}): ${stderrBuf}`,
        ),
      );
    });
  });

  const manifest = await Promise.race([
    waitForReadyFile(readyFile, timeoutMs),
    exitedEarly,
  ]);
  const client = await connectPeripherals(manifest.control_sock);

  async function stop() {
    client.close();
    if (child.exitCode === null && !child.killed) {
      child.kill('SIGTERM');
      await new Promise((resolve) => {
        const t = setTimeout(() => {
          try {
            child.kill('SIGKILL');
          } catch {}
          resolve();
        }, 3000);
        child.once('exit', () => {
          clearTimeout(t);
          resolve();
        });
      });
    }
  }

  return { manifest, client, child, stop };
}

export class PeripheralsClient {
  constructor(socket, sockPath) {
    this.socket = socket;
    this.sockPath = sockPath;
    this._seq = 0;
    this._buf = '';
    this._pending = new Map();

    socket.setEncoding('utf8');
    socket.on('data', (chunk) => {
      this._buf += chunk;
      let nl;
      while ((nl = this._buf.indexOf('\n')) >= 0) {
        const line = this._buf.slice(0, nl);
        this._buf = this._buf.slice(nl + 1);
        if (!line.trim()) continue;
        let msg;
        try {
          msg = JSON.parse(line);
        } catch {
          continue;
        }
        const entry = this._pending.get(msg.id);
        if (!entry) continue;
        this._pending.delete(msg.id);
        if (msg.ok) {
          entry.resolve(msg.result);
        } else {
          entry.reject(new Error(msg.error || `RPC error for id=${msg.id}`));
        }
      }
    });

    socket.on('close', () => {
      for (const [, entry] of this._pending) {
        entry.reject(new Error('peripherals control socket closed'));
      }
      this._pending.clear();
    });
  }

  call(method, params = {}) {
    const id = ++this._seq;
    return new Promise((resolve, reject) => {
      this._pending.set(id, { resolve, reject });
      const line = `${JSON.stringify({ id, method, params })}\n`;
      this.socket.write(line, (err) => {
        if (err) {
          this._pending.delete(id);
          reject(err);
        }
      });
    });
  }

  // Ergonomic typed helpers
  ping() { return this.call('ping'); }
  status() { return this.call('status'); }

  faceStatus() { return this.call('face.status'); }
  faceHistory(params = {}) { return this.call('face.history', params); }
  faceInjectHello(params = {}) { return this.call('face.inject_hello', params); }
  faceAssertQrAfterHello(params = {}) { return this.call('face.assert_qr_after_hello', params); }
  faceWaitFor(predicate, timeoutMs = 5000) {
    return this.call('face.wait_for', { predicate, timeout_ms: timeoutMs });
  }
  faceSetHeartbeat(params = {}) { return this.call('face.set_heartbeat', params); }
  faceInjectError(err) { return this.call('face.inject_error', { err }); }
  faceSetFault(params = {}) { return this.call('face.set_fault', params); }
  faceClearHistory() { return this.call('face.clear_history'); }

  servoStatus() { return this.call('servo.status'); }
  servoHistory(params = {}) { return this.call('servo.history', params); }
  servoWaitForGoal(params = {}) { return this.call('servo.wait_for_goal', params); }
  servoSetRegister(id, reg, bytes) { return this.call('servo.set_register', { id, reg, bytes }); }
  servoSetFault(params = {}) { return this.call('servo.set_fault', params); }
  servoAssertBounds(params = {}) { return this.call('servo.assert_bounds', params); }
  servoClearHistory() { return this.call('servo.clear_history'); }

  cameraStatus() { return this.call('camera.status'); }
  cameraSetScene(params = {}) { return this.call('camera.set_scene', params); }

  speakerStatus() { return this.call('speaker.status'); }
  speakerWaitFor(params = {}) { return this.call('speaker.wait_for', params); }
  speakerSetFault(params = {}) { return this.call('speaker.set_fault', params); }
  speakerSetCaptureDir(dir) { return this.call('speaker.set_capture_dir', { dir }); }
  speakerClearHistory() { return this.call('speaker.clear_history'); }

  micStatus() { return this.call('mic.status'); }
  micPlayFile(params) { return this.call('mic.play_file', params); }
  micPlayPcmB64(params) { return this.call('mic.play_pcm_b64', params); }
  micStop() { return this.call('mic.stop'); }
  micSetNoise(rms) { return this.call('mic.set_noise', { rms }); }
  micSetEcho(params) { return this.call('mic.set_echo', params); }
  micSetFault(params) { return this.call('mic.set_fault', params); }
  micWaitIdle(timeoutMs = 30000) { return this.call('mic.wait_idle', { timeout_ms: timeoutMs }); }

  whisperStatus() { return this.call('whisper.status'); }
  whisperConfigure(params) { return this.call('whisper.configure', params); }
  whisperRelease(callId = null) { return this.call('whisper.release', callId ? { call_id: callId } : {}); }
  whisperWaitFor(params) { return this.call('whisper.wait_for', params); }
  whisperClearHistory() { return this.call('whisper.clear_history'); }

  nmcliStatus() { return this.call('nmcli.status'); }
  nmcliSetState(state) { return this.call('nmcli.set_state', { state }); }

  close() {
    try {
      this.socket.end();
      this.socket.destroy();
    } catch {}
  }
}
