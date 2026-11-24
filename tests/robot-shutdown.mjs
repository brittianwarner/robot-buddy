// Regression check: the robot must stop promptly on SIGTERM while its stdin is still open.
// On the device stdin is the wake-word/speech-recognition pipe, which never closes, so
// systemd's stop waits for TimeoutStopSec and then SIGKILLs (no final cache flush).
//
//   cargo build --release -p robot-buddy-server -p robot-buddy-robot && bun run test:shutdown
//
// Fixed 2026-11-23: stdin is read on a plain OS thread (a blocking-pool read made runtime
// shutdown wait forever), and runtime shutdown is bounded. Each stop must also write the
// cache file (the final cursor flush), which proves the robot stopped cleanly rather than
// being killed.
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';

const root = path.resolve(import.meta.dir, '..');
const bin = (name) => path.join(process.env.CARGO_TARGET_DIR ?? path.join(root, 'target'), 'release', name);
const work = fs.mkdtempSync(path.join(os.tmpdir(), 'robot-buddy-shutdown-'));
try {
  for (const args of [
    ['init', path.join(work, 's')],
    ['provision', path.join(work, 's/issuer.key'), path.join(work, 'device.json')],
  ]) {
    if (spawnSync(bin('robot-buddy-server'), args).status !== 0) throw new Error('server init/provision failed');
  }
  const cache = path.join(work, 'cache.json');
  // The robot's first connection to its server URL comes from the session loop, which runs
  // in the same select as the stop-signal handler, so the handler is installed by then. A
  // fixed delay raced startup: before that select, SIGTERM still has its default action.
  let connected = () => {};
  const server = net.createServer((socket) => {
    socket.destroy();
    connected();
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const serverUrl = `http://127.0.0.1:${server.address().port}`;
  const started = (robot) =>
    new Promise((resolve) => {
      const give = setTimeout(resolve, 15000);
      connected = () => {
        clearTimeout(give);
        connected = () => {};
        setTimeout(resolve, 50);
      };
      robot.on('exit', resolve);
    });
  const exitAfterTerm = (closeStdin, signal = 'SIGTERM') =>
    new Promise((resolve) => {
      fs.rmSync(cache, { force: true });
      const robot = spawn(bin('robot-buddy-robot'), ['--simulate'], {
        env: {
          ...process.env,
          RUST_LOG: 'off',
          // Accepts and drops every connection: the robot stays in its reconnect loop.
          BUDDY_SERVER_URL: serverUrl,
          BUDDY_IDENTITY_FILE: path.join(work, 'device.json'),
          BUDDY_CACHE_FILE: cache,
        },
        stdio: ['pipe', 'ignore', 'ignore'],
      });
      started(robot).then(() => {
        if (closeStdin) robot.stdin.end();
        const t = Date.now();
        robot.kill(signal);
        const kill = setTimeout(() => {
          robot.kill('SIGKILL');
          resolve(null);
        }, 5000);
        robot.on('exit', (code) => {
          clearTimeout(kill);
          resolve({ ms: Date.now() - t, code, flushed: fs.existsSync(cache) });
        });
      });
    });
  // Well under systemd's TimeoutStopSec; a clean stop takes tens of milliseconds.
  const LIMIT_MS = 500;
  const runs = [
    ['SIGTERM, stdin open', false, 'SIGTERM'],
    ['SIGTERM, stdin open', false, 'SIGTERM'],
    ['SIGTERM, stdin open', false, 'SIGTERM'],
    ['SIGINT, stdin open', false, 'SIGINT'],
    ['SIGTERM, stdin closed', true, 'SIGTERM'],
  ];
  let failed = false;
  for (const [label, closeStdin, signal] of runs) {
    const result = await exitAfterTerm(closeStdin, signal);
    const ok = result !== null && result.ms < LIMIT_MS && result.code === 0 && result.flushed;
    failed ||= !ok;
    console.log(
      `${ok ? 'ok  ' : 'FAIL'} ${label}: ${
        result === null
          ? 'still running after 5000 ms'
          : `exited ${result.code} in ${result.ms} ms, cache ${result.flushed ? 'flushed' : 'NOT flushed'}`
      }`,
    );
  }
  server.close();
  process.exitCode = failed ? 1 : 0;
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
