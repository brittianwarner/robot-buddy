import { describe, expect, test } from 'bun:test';
import { evaluateSoakGate, parseRobotTimeline, redactEvidence } from './soak-gate.mjs';

const START = Date.parse('2026-11-24T19:00:00.000Z');
const DURATION_MS = 30 * 60 * 1000;
const END = START + DURATION_MS;

const esc = (s) => `\x1b[2m${s}\x1b[0m`;
const line = (ms, level, target, msg) =>
  `${esc(new Date(ms).toISOString().replace('Z', '123Z'))} \x1b[32m ${level}\x1b[0m ${esc(`${target}:`)} ${msg}`;

/**
 * Synthesizes a realistic robot.log: neck window every 5s from 60s before
 * start (pre-soak ticks must not count) to end, and a vision window every 5s.
 */
function fixtureLog({ stopMotionAt = null, gap = null, truncateAt = null, extra = [], stopVisionAt = null } = {}) {
  const out = [];
  let total = 0;
  let frames = 0;
  for (let t = START - 60_000; t <= END + 1_000; t += 5_000) {
    if (truncateAt != null && t >= truncateAt) break;
    const inGap = gap && t > gap.from && t < gap.from + gap.ms;
    const motionStopped = stopMotionAt != null && t >= stopMotionAt;
    if (!inGap && !motionStopped) {
      const ticks = gap && t >= gap.from + gap.ms && t - 5_000 < gap.from + gap.ms ? Math.round(((gap.ms + 5_000) / 1000) * 50) : 250;
      total += ticks;
      out.push(
        line(
          t,
          'INFO',
          'robot_buddy_robot::motion',
          `neck loop timing period_ms=20 ticks=${ticks} missed=0 skipped_slots=0 total_ticks=${total} total_missed=0 total_skipped_slots=0 p95_cycle_us=900 max_cycle_us=1200 max_work_us=800 max_overrun_us=0`,
        ),
      );
    }
    if (stopVisionAt == null || t < stopVisionAt) {
      frames += 50;
      out.push(line(t + 1_000, 'INFO', 'robot_buddy_robot::vision', `camera inference active vision_frames=${frames}`));
    }
  }
  for (const e of extra) out.push(e);
  return `${out.join('\n')}\n`;
}

/** One peripheral sample every 12s with a healthy servo/camera/thread snapshot. */
function fixtureSamples({ torqueOffAt = null, threadGoneAt = null, dropServoAt = null, servoStopAt = null } = {}) {
  const samples = [];
  for (let t = START; t <= END; t += 12_000) {
    const servoMs = servoStopAt != null ? Math.min(t, servoStopAt) : t;
    const s = {
      wall_ms: t,
      robot: { pid: 106, rss_kib: 190_000 + Math.floor((t - START) / 60_000), threads: 10 },
      robot_tree_rss_kib: 212_000,
      robot_threads: ['robot-buddy-rob', 'tokio-rt-worker', 'journal', 'control-input', 'neck-50hz'],
      servo: {
        goal_frames_count: Math.round(((servoMs - START) / 1000) * 50) + 3_000,
        all_torque_on: !(torqueOffAt != null && t >= torqueOffAt),
        torques: torqueOffAt != null && t >= torqueOffAt ? [0, 0, 0] : [1, 1, 1],
      },
      camera: { frames_sent: Math.round(((t - START) / 1000) * 10) + 100 },
    };
    if (threadGoneAt != null && t >= threadGoneAt) s.robot_threads = s.robot_threads.filter((n) => n !== 'neck-50hz');
    if (dropServoAt != null && Math.abs(t - dropServoAt) < 6_000) s.servo = null;
    samples.push(s);
  }
  // Final sample taken at end.
  const last = samples.at(-1);
  if (last.wall_ms !== END) samples.push({ ...structuredClone(last), wall_ms: END });
  return samples;
}

const gate = (opts = {}) =>
  evaluateSoakGate({
    robotLog: opts.robotLog ?? fixtureLog(opts.log),
    samples: opts.samples ?? fixtureSamples(opts.samples_opts),
    startMs: START,
    endMs: END,
    includeVision: true,
  });
const kinds = (r) => [...new Set(r.failures.map((f) => f.kind))];

describe('soak gate', () => {
  test('healthy 30m fixture passes with full coverage and first/mid/end evidence', () => {
    const r = gate();
    expect(r.failures).toEqual([]);
    expect(r.ok).toBe(true);
    expect(r.motion.stats.windows_in_run).toBe(360);
    expect(r.motion.stats.run_coverage_pct).toBeGreaterThanOrEqual(99);
    expect(r.motion.stats.min_window_hz).toBe(50);
    expect(r.samples.stats.first.robot_rss_kib).not.toBeNull();
    expect(r.samples.stats.mid.robot_rss_kib).not.toBeNull();
    expect(r.samples.stats.end.at).toBe(new Date(END).toISOString());
  });

  test('neck stopping in the last 3 minutes fails even though 90% coverage holds', () => {
    const r = gate({ log: { stopMotionAt: END - 3 * 60_000 } });
    expect(r.ok).toBe(false);
    expect(r.motion.stats.run_coverage_pct).toBeGreaterThan(89);
    expect(kinds(r)).toContain('motion_stale_at_end');
  });

  test('a 10s telemetry gap fails', () => {
    const r = gate({ log: { gap: { from: START + 900_000, ms: 10_000 } } });
    expect(r.ok).toBe(false);
    expect(kinds(r)).toContain('motion_gap');
  });

  test('a truncated log (writes stop at 20 min, partial last line) fails', () => {
    const cut = fixtureLog({ truncateAt: START + 20 * 60_000 });
    const robotLog = `${cut}${line(START + 20 * 60_000, 'INFO', 'robot_buddy_robot::motion', 'neck loop timing period_ms=20 ticks=25')}`;
    const r = gate({ robotLog });
    expect(r.ok).toBe(false);
    expect(r.log_integrity.partial_tail_bytes).toBeGreaterThan(0);
    expect(kinds(r)).toEqual(expect.arrayContaining(['motion_stale_at_end', 'vision_stale_at_end']));
  });

  test('a malformed complete telemetry line fails explicitly', () => {
    const extra = [line(START + 100_000, 'INFO', 'robot_buddy_robot::motion', 'neck loop timing period_ms=20 ticks=250')];
    const r = gate({ log: { extra } });
    expect(kinds(r)).toContain('log_malformed_telemetry');
  });

  test('a neck motion stopped line fails and is stored redacted', () => {
    const extra = [
      line(
        START + 400_000,
        'ERROR',
        'robot_buddy_robot::motion',
        'neck motion stopped; restart requires verification error=serial read timeout token=abcdefabcdefabcdefabcdef0123',
      ),
    ];
    const r = gate({ log: { extra } });
    expect(r.ok).toBe(false);
    const ev = r.failures.find((f) => f.kind === 'neck_stop_event');
    expect(ev.at).toBe(new Date(START + 400_000).toISOString());
    expect(ev.detail).toContain('neck motion stopped');
    expect(ev.detail).not.toContain('abcdefabcdef');
  });

  test('counter reset (worker restarted) fails', () => {
    const base = fixtureLog().split('\n');
    const idx = base.findIndex((l) => l.includes('total_ticks=') && Date.parse(l.slice(4, 31)) > START + 600_000);
    base[idx] = base[idx].replace(/total_ticks=\d+/, 'total_ticks=250');
    const r = gate({ robotLog: base.join('\n') });
    expect(kinds(r)).toContain('motion_counter_reset');
  });

  test('vision stopping late fails', () => {
    const r = gate({ log: { stopVisionAt: END - 120_000 } });
    expect(kinds(r)).toContain('vision_stale_at_end');
  });

  test('servo torque drop in any sample fails', () => {
    const r = gate({ samples_opts: { torqueOffAt: END - 60_000 } });
    expect(kinds(r)).toContain('servo_torque_off');
  });

  test('servo goal frames stalling between samples fails', () => {
    const r = gate({ samples_opts: { servoStopAt: END - 150_000 } });
    expect(kinds(r)).toContain('servo_low_rate');
  });

  test('neck-50hz thread disappearing from /proc fails', () => {
    const r = gate({ samples_opts: { threadGoneAt: START + 387_000 } });
    expect(kinds(r)).toContain('robot_thread_missing');
  });

  test('a sample missing servo data fails explicitly', () => {
    const r = gate({ samples_opts: { dropServoAt: START + 600_000 } });
    expect(kinds(r)).toContain('sample_missing_data');
    expect(r.samples.stats.missing_data_samples).toBeGreaterThan(0);
  });

  test('empty log fails rather than passing vacuously', () => {
    const r = gate({ robotLog: '' });
    expect(kinds(r)).toEqual(expect.arrayContaining(['motion_no_data', 'vision_no_data']));
  });
});

describe('timeline + redaction', () => {
  test('pre-soak windows are a baseline only, never in-run coverage', () => {
    const t = parseRobotTimeline(fixtureLog());
    expect(t.motion_windows.filter((w) => w.ts <= START).length).toBe(13);
  });

  test('redactEvidence strips URLs, tokens, codes, and key material', () => {
    const out = redactEvidence(
      'claim https://x.test/claim?bot=a&token=zzz code=123456 session: eyJhbGciOi.eyJzdWIi.sig -----BEGIN PRIVATE KEY----- abc -----END PRIVATE KEY-----',
    );
    expect(out).not.toContain('token=zzz');
    expect(out).not.toContain('123456');
    expect(out).not.toContain('eyJhbGciOi');
    expect(out).not.toContain('BEGIN PRIVATE KEY');
  });
});
