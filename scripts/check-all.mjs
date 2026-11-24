#!/usr/bin/env bun
// The full local gate, in CI order, stopping at the first failure:
//
//   bun run check:all
//   BUDDY_TEST_ENGINE_PORT=16920 BUDDY_TEST_MODEL_PORT=19591 BENCH_PORT_BASE=18000 bun run check:all
//   bun run check:all -- --from integration     # resume at a step (names in the table)
//
// Every step inherits this process's environment, so the port variables
// (BUDDY_TEST_ENGINE_PORT, BUDDY_TEST_MODEL_PORT, BENCH_PORT_BASE, BUDDY_SCREEN_ADDR) and
// BUDDY_STRICT_TIMING reach each harness unchanged. Ends with a table of every step, its
// result and its wall time; exit 1 when a step failed, 2 for an unknown --from.
import { spawn } from 'node:child_process';
import path from 'node:path';

const root = path.resolve(import.meta.dir, '..');
const STEPS = [
  ['fmt', 'cargo', ['fmt', '--all', '--check']],
  ['clippy', 'cargo', ['clippy', '--workspace', '--all-targets', '--all-features', '--locked', '--', '-D', 'warnings']],
  ['cargo test', 'cargo', ['test', '--workspace', '--locked']],
  ['pip full features', 'cargo', ['test', '-p', 'robot-buddy-robot', '--all-features', '--locked']],
  ['test:robot-features', 'bun', ['run', 'test:robot-features']],
  ['deny', 'bun', ['run', 'deny']],
  ['integration', 'bun', ['run', 'test:integration']],
  ['release build', 'cargo', ['build', '--release', '--workspace', '--locked']],
  ['shutdown', 'bun', ['run', 'test:shutdown']],
  ['reconnect --strict', 'bun', ['tests/robot-reconnect.mjs', '--no-build', '--strict']],
  ['bench --no-run', 'cargo', ['bench', '--workspace', '--locked', '--no-run']],
  ['early-player', 'bun', ['tests/early-player.mjs']],
  ['lab-reconnect', 'bun', ['run', 'test:lab-reconnect']],
  ['eval:voice', 'bun', ['tests/eval-voice.mjs', '--no-build']],
  ['bench:report:check', 'bun', ['run', 'bench:report:check']],
];

const argv = process.argv.slice(2);
const fromIndex = argv.indexOf('--from');
const from = fromIndex >= 0 ? argv[fromIndex + 1] : null;
const start = from ? STEPS.findIndex(([name]) => name === from) : 0;
if (start < 0) {
  console.error(`check:all: no step named ${JSON.stringify(from)}; steps: ${STEPS.map(([n]) => n).join(', ')}`);
  process.exit(2);
}

const shown = ['BUDDY_TEST_ENGINE_PORT', 'BUDDY_TEST_MODEL_PORT', 'BENCH_PORT_BASE', 'BUDDY_SCREEN_ADDR', 'BUDDY_STRICT_TIMING'];
const settings = shown.filter((k) => process.env[k] !== undefined).map((k) => `${k}=${process.env[k]}`);
console.error(`check:all: ${settings.length ? settings.join(' ') : 'default ports'}`);

let child = null;
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    child?.kill(signal);
    process.exit(130);
  });
}

const results = STEPS.map(([name]) => ({ name, status: 'skipped', seconds: null }));
for (let i = 0; i < start; i++) results[i].status = 'not run (--from)';
let failed = false;
for (let i = start; i < STEPS.length; i++) {
  const [name, cmd, args] = STEPS[i];
  console.error(`\ncheck:all: [${i + 1}/${STEPS.length}] ${name}: ${cmd} ${args.join(' ')}`);
  const t = performance.now();
  child = spawn(cmd, args, { cwd: root, env: process.env, stdio: 'inherit' });
  const code = await new Promise((resolve) => {
    child.on('error', () => resolve(127));
    child.on('exit', (c, s) => resolve(c ?? (s ? 1 : 0)));
  });
  child = null;
  results[i] = { name, status: code === 0 ? 'ok' : `FAIL (exit ${code})`, seconds: (performance.now() - t) / 1000 };
  if (code !== 0) {
    failed = true;
    break;
  }
}

const width = Math.max(...results.map((r) => r.name.length));
const total = results.reduce((a, r) => a + (r.seconds ?? 0), 0);
console.error('\ncheck:all summary');
for (const r of results) {
  const secs = r.seconds === null ? '' : `${r.seconds.toFixed(1).padStart(7)} s`;
  console.error(`  ${r.name.padEnd(width)}  ${r.status.padEnd(16)} ${secs}`);
}
console.error(`  ${'total'.padEnd(width)}  ${(failed ? 'FAILED' : 'passed').padEnd(16)} ${total.toFixed(1).padStart(7)} s`);
process.exit(failed ? 1 : 0);
