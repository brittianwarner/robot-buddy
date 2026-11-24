// Regression check: the robot must stop promptly on SIGTERM while its stdin is still open.
// On the device stdin is the wake-word/speech-recognition pipe, which never closes, so
// systemd's stop waits for TimeoutStopSec and then SIGKILLs (no final cache flush).
//
//   cargo build --release -p robot-buddy-robot && bun tests/robot-shutdown.mjs
//
// KNOWN FAILING (2026-11-23): main() returns after the signal, but dropping the Tokio
// runtime waits for the blocking stdin reader (tokio::io::stdin), which never returns.
// Closing stdin first makes it exit in milliseconds. Not wired into CI until fixed.
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
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
  const exitAfterTerm = (closeStdin) =>
    new Promise((resolve) => {
      const robot = spawn(bin('robot-buddy-robot'), ['--simulate'], {
        env: {
          ...process.env,
          RUST_LOG: 'off',
          // Nothing listens here: the robot stays in its reconnect loop.
          BUDDY_SERVER_URL: 'http://127.0.0.1:9',
          BUDDY_IDENTITY_FILE: path.join(work, 'device.json'),
          BUDDY_CACHE_FILE: path.join(work, 'cache.json'),
        },
        stdio: ['pipe', 'ignore', 'ignore'],
      });
      setTimeout(() => {
        if (closeStdin) robot.stdin.end();
        const t = Date.now();
        robot.kill('SIGTERM');
        const kill = setTimeout(() => {
          robot.kill('SIGKILL');
          resolve(null);
        }, 5000);
        robot.on('exit', () => {
          clearTimeout(kill);
          resolve(Date.now() - t);
        });
      }, 1500);
    });
  const open = await exitAfterTerm(false);
  const closed = await exitAfterTerm(true);
  console.log(`SIGTERM with stdin open: ${open === null ? 'still running after 5000 ms' : `exited in ${open} ms`}`);
  console.log(`SIGTERM with stdin closed: ${closed === null ? 'still running after 5000 ms' : `exited in ${closed} ms`}`);
  process.exitCode = open !== null && open < 2000 ? 0 : 1;
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
