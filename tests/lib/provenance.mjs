// Where and on what a bench run was measured, and whether the host was quiet enough for
// its numbers to be published. tests/bench-e2e.mjs records all three in its JSON;
// `bun run bench:report:check` (tests/e2e-report.mjs --check) refuses a published artifact
// that is dirty, noisy, or measured on sources that differ from HEAD.
import { spawnSync } from 'node:child_process';
import os from 'node:os';

const git = (repo, ...args) => spawnSync('git', ['-C', repo, ...args], { encoding: 'utf8' });

/** The commit a run measures: full SHA, `git describe`, and whether the tree was dirty. */
export function gitState(repo) {
  const sha = git(repo, 'rev-parse', 'HEAD').stdout.trim() || null;
  const describe = git(repo, 'describe', '--always', '--dirty').stdout.trim() || null;
  // Untracked files count: a new source file changes what is built.
  const status = git(repo, 'status', '--porcelain', '--untracked-files=normal');
  const dirty = status.status !== 0 ? null : status.stdout.trim().length > 0;
  return { sha, describe, dirty };
}

/** The host: enough to tell two machines, or one machine under two loads, apart. */
export function machine() {
  const cpus = os.cpus();
  return {
    platform: os.platform(),
    arch: os.arch(),
    release: os.release(),
    cpu: cpus[0]?.model ?? null,
    cpus: cpus.length,
    memory_gb: Math.round(os.totalmem() / 2 ** 30),
    bun: Bun.version,
    rustc: spawnSync('rustc', ['--version'], { encoding: 'utf8' }).stdout.trim() || null,
  };
}

export const loadAverage = () => os.loadavg().map((l) => Math.round(l * 10) / 10);

/**
 * Publishing limits. The fakes keep an absolute schedule and record every frame they send
 * 20 ms or more late; on a quiet host that never happens, so lateness measures the host's
 * own noise independently of the code under test. Load is the 1-minute average per CPU,
 * at the start and at the end of the run.
 */
export const NOISE_LIMITS = { load_per_cpu: 0.5, fake_late_frames: 5, fake_late_max_ms: 50 };

/**
 * Noise verdict for one run: `{ ok, reasons, load_per_cpu, fake_late_frames,
 * fake_late_max_ms, limits }`. `late` is every fake frame that went out late (`{ ms }`).
 */
export function noise({ loadStart, loadEnd, cpus, late }) {
  const perCpu = Math.round((Math.max(loadStart[0], loadEnd[0]) / Math.max(1, cpus)) * 100) / 100;
  const lateMax = late.length ? Math.round(Math.max(...late.map((l) => l.ms))) : 0;
  const reasons = [];
  if (perCpu > NOISE_LIMITS.load_per_cpu) reasons.push(`load ${perCpu} per CPU > ${NOISE_LIMITS.load_per_cpu}`);
  if (late.length > NOISE_LIMITS.fake_late_frames) {
    reasons.push(`fakes sent ${late.length} frames late > ${NOISE_LIMITS.fake_late_frames}`);
  }
  if (lateMax > NOISE_LIMITS.fake_late_max_ms) reasons.push(`a fake frame went out ${lateMax} ms late > ${NOISE_LIMITS.fake_late_max_ms}`);
  return {
    ok: reasons.length === 0,
    reasons,
    load_per_cpu: perCpu,
    fake_late_frames: late.length,
    fake_late_max_ms: lateMax,
    limits: NOISE_LIMITS,
  };
}

/**
 * Paths whose content decides what the bench measures. Docs, Markdown and the Rust test
 * and bench targets do not; a published artifact stays valid across changes to them.
 */
export const MEASURED_PATHS = [
  'apps',
  'crates',
  'tests/lib',
  'tests/bench-e2e.mjs',
  'Cargo.toml',
  'Cargo.lock',
  'rust-toolchain.toml',
  ':(exclude,glob)**/*.md',
  ':(exclude)crates/buddy/tests',
  ':(exclude)crates/buddy/benches',
  ':(exclude)apps/server/tests',
];

/**
 * Why a published artifact does not describe `repo` at HEAD, or `[]` when it does: it
 * must name a clean commit, pass its noise check, and have no measured path changed
 * between that commit and HEAD.
 */
export function staleReasons(run, repo) {
  const reasons = [];
  const sha = run.git?.sha;
  if (!sha) return ['the artifact records no commit SHA (published before provenance was recorded); republish'];
  if (run.git.dirty !== false) reasons.push(`measured on a dirty tree (${run.git.describe ?? sha})`);
  if (!run.noise) reasons.push('the artifact records no noise check');
  else if (!run.noise.ok) reasons.push(`measured on a noisy host: ${run.noise.reasons.join('; ')}`);
  if (git(repo, 'cat-file', '-e', `${sha}^{commit}`).status !== 0) {
    reasons.push(`commit ${sha} is not in this clone (a shallow checkout needs fetch-depth: 0)`);
    return reasons;
  }
  const changed = git(repo, 'diff', '--name-only', sha, 'HEAD', '--', ...MEASURED_PATHS).stdout.trim();
  if (changed) {
    const files = changed.split('\n');
    const shown = files.slice(0, 10).join(', ');
    reasons.push(
      `measured sources changed since ${sha.slice(0, 12)}: ${shown}${files.length > 10 ? `, and ${files.length - 10} more` : ''}`,
    );
  }
  return reasons;
}
