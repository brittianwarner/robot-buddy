// Pure sustained-motion soak gate for the ARM64 Pip E2E harness.
//
// Everything here is a deterministic function of (robot.log text, periodic
// samples, start/end wall clock) so fixture tests can prove the gate fails on
// a late neck stop, a telemetry gap, a truncated log, or missing samples. No
// I/O, no clocks, no secrets: evidence carries timestamps, counters, and
// redacted error templates only.

const ANSI_RE = /\x1b\[[0-9;]*m/g;
const TS_RE = /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z)\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+(\S+?):?\s+(.*)$/;

export const NECK_HZ = 50;

/** Default thresholds. Every value is derived from the product cadence, not tuned to a run. */
export const DEFAULT_THRESHOLDS = Object.freeze({
  // neck loop and vision both log a window every 5s (motion TIMING_REPORT_INTERVAL,
  // vision INFERENCE_ACTIVITY_LOG_INTERVAL); 1.5x is the largest tolerated gap.
  motion_window_max_gap_ms: 7_500,
  motion_final_freshness_ms: 7_500,
  vision_window_max_gap_ms: 7_500,
  vision_final_freshness_ms: 7_500,
  // Every window and every sample interval must sustain >= 90% of 50 Hz.
  min_rate_fraction: 0.9,
  // Whole-run tick coverage (in-run windows only; pre-soak ticks never count).
  min_run_coverage_fraction: 0.9,
  // Peripheral samples are per turn (~8-20s); a per-interval rate check covers
  // whatever gap occurs, this bounds how coarse that check may become.
  sample_max_gap_ms: 180_000,
  sample_final_freshness_ms: 180_000,
  required_threads: ['neck-50hz'],
});

/** Strip anything that could carry a credential; keep the message template and short scalars. */
export function redactEvidence(text) {
  return String(text ?? '')
    .replace(ANSI_RE, '')
    .replace(/https?:\/\/[^\s"']+/gi, '<url>')
    .replace(/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, '<email>')
    .replace(/-----BEGIN[^-]*-----[\s\S]*?(-----END[^-]*-----|$)/g, '<pem>')
    .replace(/\b(?:eyJ[A-Za-z0-9_-]+\.){1,2}[A-Za-z0-9_-]*/g, '<jwt>')
    .replace(/\b[A-Za-z0-9+/_=-]{24,}\b/g, '<token>')
    .replace(/\b[0-9a-fA-F]{16,}\b/g, '<hex>')
    .replace(/(?<!\d)\d{6}(?!\d)/g, '<6digits>')
    .replace(/((?:token|secret|password|key|session|code|authorization)\s*[=:]\s*)\S+/gi, '$1<redacted>')
    .trim()
    .slice(0, 240);
}

const kvNum = (rest, key) => {
  const m = new RegExp(`(?:^|\\s)${key}=(\\d+)(?=\\s|$)`).exec(rest);
  return m ? Number(m[1]) : null;
};

/**
 * Parses robot.log into a timestamped timeline. A final segment without a
 * trailing newline is a partial write and is reported, never parsed.
 */
export function parseRobotTimeline(rawText) {
  const text = String(rawText ?? '').replace(ANSI_RE, '');
  const segments = text.split('\n');
  const partialTail = segments.pop() ?? '';
  const timeline = {
    lines: segments.length,
    partial_tail_bytes: partialTail.length,
    untimestamped_lines: 0,
    motion_windows: [],
    vision_windows: [],
    stop_events: [],
    camera_errors: [],
    panics: [],
    malformed: [],
  };
  for (const line of segments) {
    if (!line.trim()) continue;
    const m = TS_RE.exec(line);
    if (!m) {
      if (/panicked at|thread '.*' panicked|fatal runtime error|SIGSEGV|SIGABRT/i.test(line)) {
        timeline.panics.push({ ts: null, message: redactEvidence(line) });
      } else {
        timeline.untimestamped_lines += 1;
      }
      continue;
    }
    const ts = Date.parse(m[1]);
    const level = m[2];
    const rest = m[4];
    if (/panicked at|thread '.*' panicked|fatal runtime error|SIGSEGV|SIGABRT/i.test(rest)) {
      timeline.panics.push({ ts, message: redactEvidence(rest) });
    }
    if (
      rest.includes('neck motion stopped') ||
      rest.includes('neck worker panicked') ||
      rest.includes('could not confirm neck torque off') ||
      rest.includes('neck disarmed')
    ) {
      timeline.stop_events.push({ ts, level, message: redactEvidence(rest) });
    }
    if (rest.includes('camera stopped') || rest.includes('camera error')) {
      timeline.camera_errors.push({ ts, level, message: redactEvidence(rest) });
    }
    if (rest.startsWith('neck loop timing') || rest.startsWith('neck loop missed 20ms deadline')) {
      const w = {
        ts,
        ticks: kvNum(rest, 'ticks'),
        missed: kvNum(rest, 'missed'),
        skipped_slots: kvNum(rest, 'skipped_slots'),
        total_ticks: kvNum(rest, 'total_ticks'),
        total_missed: kvNum(rest, 'total_missed'),
      };
      if (w.ticks == null || w.total_ticks == null || w.missed == null || w.total_missed == null) {
        timeline.malformed.push({ ts, kind: 'motion', message: redactEvidence(rest) });
      } else {
        timeline.motion_windows.push(w);
      }
    }
    if (rest.startsWith('camera inference active')) {
      const vf = kvNum(rest, 'vision_frames');
      if (vf == null) timeline.malformed.push({ ts, kind: 'vision', message: redactEvidence(rest) });
      else timeline.vision_windows.push({ ts, vision_frames: vf });
    }
  }
  return timeline;
}

const iso = (ms) => (Number.isFinite(ms) ? new Date(ms).toISOString() : null);

/** Gap + freshness walk over timestamped points bounded by [startMs, endMs]. */
function cadenceFailures({ label, points, startMs, endMs, maxGapMs, finalFreshnessMs }) {
  const failures = [];
  const inRun = points.filter((p) => p.ts > startMs && p.ts <= endMs);
  if (inRun.length === 0) {
    failures.push({ kind: `${label}_no_data`, detail: `no ${label} windows between start and end` });
    return { failures, inRun };
  }
  if (inRun[0].ts - startMs > maxGapMs) {
    failures.push({
      kind: `${label}_late_first`,
      at: iso(inRun[0].ts),
      detail: `first ${label} window ${inRun[0].ts - startMs}ms after start (max ${maxGapMs})`,
    });
  }
  for (let i = 1; i < inRun.length; i += 1) {
    const gap = inRun[i].ts - inRun[i - 1].ts;
    if (gap > maxGapMs) {
      failures.push({
        kind: `${label}_gap`,
        at: iso(inRun[i - 1].ts),
        detail: `${gap}ms gap ${iso(inRun[i - 1].ts)} -> ${iso(inRun[i].ts)} (max ${maxGapMs})`,
      });
    }
  }
  const staleness = endMs - inRun.at(-1).ts;
  if (staleness > finalFreshnessMs) {
    failures.push({
      kind: `${label}_stale_at_end`,
      at: iso(inRun.at(-1).ts),
      detail: `last ${label} window ${staleness}ms before end (max ${finalFreshnessMs})`,
    });
  }
  return { failures, inRun };
}

export function evaluateMotion({ timeline, startMs, endMs, thresholds = DEFAULT_THRESHOLDS }) {
  const minRate = NECK_HZ * thresholds.min_rate_fraction;
  const { failures, inRun } = cadenceFailures({
    label: 'motion',
    points: timeline.motion_windows,
    startMs,
    endMs,
    maxGapMs: thresholds.motion_window_max_gap_ms,
    finalFreshnessMs: thresholds.motion_final_freshness_ms,
  });
  // Baseline: last window at or before start, so the first in-run window has a difference too.
  const baseline = [...timeline.motion_windows].reverse().find((w) => w.ts <= startMs) ?? null;
  const series = baseline ? [baseline, ...inRun] : inRun;
  const perWindow = [];
  let minWindowRate = null;
  for (let i = 1; i < series.length; i += 1) {
    const prev = series[i - 1];
    const cur = series[i];
    const dTicks = cur.total_ticks - prev.total_ticks;
    const dMs = cur.ts - prev.ts;
    const rate = dMs > 0 ? (dTicks * 1000) / dMs : 0;
    perWindow.push({
      t: iso(cur.ts),
      d_ticks: dTicks,
      d_ms: dMs,
      hz: Math.round(rate * 10) / 10,
      missed: cur.missed,
    });
    if (dTicks < 0) {
      failures.push({ kind: 'motion_counter_reset', at: iso(cur.ts), detail: `total_ticks ${prev.total_ticks} -> ${cur.total_ticks}` });
    } else if (dTicks !== cur.ticks) {
      failures.push({
        kind: 'motion_counter_mismatch',
        at: iso(cur.ts),
        detail: `total_ticks advanced ${dTicks} but window reports ticks=${cur.ticks}`,
      });
    }
    if (rate < minRate) {
      failures.push({ kind: 'motion_low_rate', at: iso(cur.ts), detail: `${rate.toFixed(1)}Hz over ${dMs}ms (min ${minRate}Hz)` });
    }
    minWindowRate = minWindowRate == null ? rate : Math.min(minWindowRate, rate);
  }
  const elapsedS = (endMs - startMs) / 1000;
  const runTicks = inRun.reduce((acc, w) => acc + w.ticks, 0);
  const expected = Math.round(elapsedS * NECK_HZ);
  const coverage = expected > 0 ? runTicks / expected : 0;
  if (coverage < thresholds.min_run_coverage_fraction) {
    failures.push({
      kind: 'motion_low_coverage',
      detail: `${runTicks}/${expected} in-run ticks (${(coverage * 100).toFixed(1)}%, min ${thresholds.min_run_coverage_fraction * 100}%)`,
    });
  }
  for (const ev of timeline.stop_events) {
    failures.push({ kind: 'neck_stop_event', at: iso(ev.ts), detail: ev.message });
  }
  const missedInRun = inRun.reduce((acc, w) => acc + w.missed, 0);
  return {
    ok: failures.length === 0,
    failures,
    stats: {
      windows_in_run: inRun.length,
      baseline_window_at: iso(baseline?.ts),
      first_window_at: iso(inRun[0]?.ts),
      last_window_at: iso(inRun.at(-1)?.ts),
      run_ticks: runTicks,
      expected_ticks_50hz: expected,
      run_coverage_pct: Math.round(coverage * 1000) / 10,
      min_window_hz: minWindowRate == null ? null : Math.round(minWindowRate * 10) / 10,
      missed_in_run: missedInRun,
      missed_rate_pct: runTicks > 0 ? Math.round((missedInRun / runTicks) * 10000) / 100 : null,
      stop_events: timeline.stop_events.length,
    },
    per_window: perWindow,
  };
}

export function evaluateVision({ timeline, startMs, endMs, thresholds = DEFAULT_THRESHOLDS }) {
  const { failures, inRun } = cadenceFailures({
    label: 'vision',
    points: timeline.vision_windows,
    startMs,
    endMs,
    maxGapMs: thresholds.vision_window_max_gap_ms,
    finalFreshnessMs: thresholds.vision_final_freshness_ms,
  });
  for (let i = 1; i < inRun.length; i += 1) {
    if (inRun[i].vision_frames <= inRun[i - 1].vision_frames) {
      failures.push({
        kind: 'vision_frames_not_advancing',
        at: iso(inRun[i].ts),
        detail: `vision_frames ${inRun[i - 1].vision_frames} -> ${inRun[i].vision_frames}`,
      });
    }
  }
  for (const ev of timeline.camera_errors) {
    if (ev.ts > startMs && ev.ts <= endMs) {
      failures.push({ kind: 'camera_error_event', at: iso(ev.ts), detail: ev.message });
    }
  }
  return {
    ok: failures.length === 0,
    failures,
    stats: {
      windows_in_run: inRun.length,
      first_window_at: iso(inRun[0]?.ts),
      last_window_at: iso(inRun.at(-1)?.ts),
      frames_first: inRun[0]?.vision_frames ?? null,
      frames_last: inRun.at(-1)?.vision_frames ?? null,
    },
  };
}

/**
 * Per-sample peripheral evidence: servo goal-frame counter difference and rate
 * between every consecutive pair, torque on in every sample, camera frames
 * advancing, required thread names present, and no missing fields.
 */
export function evaluateSamples({ samples, startMs, endMs, includeVision = true, thresholds = DEFAULT_THRESHOLDS }) {
  const failures = [];
  const minRate = NECK_HZ * thresholds.min_rate_fraction;
  const missing = [];
  for (const [i, s] of samples.entries()) {
    const absent = [];
    if (!Number.isFinite(s?.wall_ms)) absent.push('wall_ms');
    if (!s?.robot || s.robot.rss_kib == null) absent.push('robot_rss');
    if (!s?.servo || !Number.isFinite(s.servo.goal_frames_count)) absent.push('servo');
    if (includeVision && (!s?.camera || !Number.isFinite(s.camera.frames_sent))) absent.push('camera');
    if (!Array.isArray(s?.robot_threads)) absent.push('robot_threads');
    if (absent.length > 0) missing.push({ index: i, at: iso(s?.wall_ms), absent });
  }
  for (const m of missing) {
    failures.push({ kind: 'sample_missing_data', at: m.at, detail: `sample ${m.index} missing ${m.absent.join(',')}` });
  }
  const valid = samples.filter((s) => Number.isFinite(s?.wall_ms));
  if (valid.length < 2) {
    failures.push({ kind: 'samples_insufficient', detail: `${valid.length} timestamped samples` });
  } else {
    if (valid[0].wall_ms - startMs > thresholds.sample_max_gap_ms) {
      failures.push({ kind: 'sample_late_first', at: iso(valid[0].wall_ms), detail: `${valid[0].wall_ms - startMs}ms after start` });
    }
    if (endMs - valid.at(-1).wall_ms > thresholds.sample_final_freshness_ms) {
      failures.push({ kind: 'sample_stale_at_end', at: iso(valid.at(-1).wall_ms), detail: `${endMs - valid.at(-1).wall_ms}ms before end` });
    }
  }
  let minServoHz = null;
  for (let i = 1; i < valid.length; i += 1) {
    const prev = valid[i - 1];
    const cur = valid[i];
    const dMs = cur.wall_ms - prev.wall_ms;
    if (dMs > thresholds.sample_max_gap_ms) {
      failures.push({ kind: 'sample_gap', at: iso(prev.wall_ms), detail: `${dMs}ms between samples` });
    }
    const pg = prev.servo?.goal_frames_count;
    const cg = cur.servo?.goal_frames_count;
    if (Number.isFinite(pg) && Number.isFinite(cg) && dMs > 0) {
      const d = cg - pg;
      const hz = (d * 1000) / dMs;
      minServoHz = minServoHz == null ? hz : Math.min(minServoHz, hz);
      if (d <= 0 || hz < minRate) {
        failures.push({ kind: 'servo_low_rate', at: iso(cur.wall_ms), detail: `goal frames +${d} over ${dMs}ms (${hz.toFixed(1)}Hz, min ${minRate}Hz)` });
      }
    }
    if (includeVision) {
      const pc = prev.camera?.frames_sent;
      const cc = cur.camera?.frames_sent;
      if (Number.isFinite(pc) && Number.isFinite(cc) && cc <= pc) {
        failures.push({ kind: 'camera_frames_not_advancing', at: iso(cur.wall_ms), detail: `frames_sent ${pc} -> ${cc}` });
      }
    }
  }
  const torqueDrops = valid.filter((s) => s.servo && s.servo.all_torque_on !== true);
  for (const s of torqueDrops.slice(0, 10)) {
    failures.push({ kind: 'servo_torque_off', at: iso(s.wall_ms), detail: `torques=${JSON.stringify(s.servo.torques ?? null)}` });
  }
  const threadDrops = [];
  for (const s of valid) {
    if (!Array.isArray(s.robot_threads)) continue;
    const absent = thresholds.required_threads.filter((t) => !s.robot_threads.includes(t));
    if (absent.length > 0) threadDrops.push({ at: iso(s.wall_ms), absent, threads: s.robot_threads.length });
  }
  for (const d of threadDrops.slice(0, 10)) {
    failures.push({ kind: 'robot_thread_missing', at: d.at, detail: `missing ${d.absent.join(',')} (${d.threads} threads)` });
  }
  const pick = (s) =>
    s
      ? {
          at: iso(s.wall_ms),
          robot_rss_kib: s.robot?.rss_kib ?? null,
          robot_tree_rss_kib: s.robot_tree_rss_kib ?? null,
          robot_threads: s.robot_threads ?? null,
          servo_goal_frames: s.servo?.goal_frames_count ?? null,
          camera_frames_sent: s.camera?.frames_sent ?? null,
        }
      : null;
  return {
    ok: failures.length === 0,
    failures,
    stats: {
      samples: samples.length,
      missing_data_samples: missing.length,
      torque_dropped_samples: torqueDrops.length,
      thread_missing_samples: threadDrops.length,
      min_servo_interval_hz: minServoHz == null ? null : Math.round(minServoHz * 10) / 10,
      first: pick(valid[0]),
      mid: pick(valid[Math.floor(valid.length / 2)]),
      end: pick(valid.at(-1)),
    },
  };
}

/** Full sustained-soak gate: log timeline + peripheral samples + log integrity. */
export function evaluateSoakGate({
  robotLog,
  samples,
  startMs,
  endMs,
  includeVision = true,
  thresholds = DEFAULT_THRESHOLDS,
}) {
  const timeline = parseRobotTimeline(robotLog);
  const integrity = [];
  if (timeline.malformed.length > 0) {
    for (const m of timeline.malformed.slice(0, 10)) {
      integrity.push({ kind: 'log_malformed_telemetry', at: iso(m.ts), detail: m.message });
    }
  }
  if (timeline.panics.length > 0) {
    for (const p of timeline.panics.slice(0, 10)) {
      integrity.push({ kind: 'panic', at: iso(p.ts), detail: p.message });
    }
  }
  if (!Number.isFinite(startMs) || !Number.isFinite(endMs) || endMs <= startMs) {
    integrity.push({ kind: 'clock_invalid', detail: `start=${startMs} end=${endMs}` });
  }
  const motion = evaluateMotion({ timeline, startMs, endMs, thresholds });
  const vision = includeVision
    ? evaluateVision({ timeline, startMs, endMs, thresholds })
    : { ok: true, failures: [], stats: { skipped: true } };
  const sampleEval = evaluateSamples({ samples, startMs, endMs, includeVision, thresholds });
  const failures = [...integrity, ...motion.failures, ...vision.failures, ...sampleEval.failures];
  return {
    ok: failures.length === 0,
    clocks: { start: iso(startMs), end: iso(endMs), elapsed_s: Math.round((endMs - startMs) / 1000) },
    thresholds: { ...thresholds },
    log_integrity: {
      lines: timeline.lines,
      partial_tail_bytes: timeline.partial_tail_bytes,
      untimestamped_lines: timeline.untimestamped_lines,
      malformed_telemetry_lines: timeline.malformed.length,
      panics: timeline.panics.length,
    },
    motion: { ok: motion.ok, stats: motion.stats },
    vision: { ok: vision.ok, stats: vision.stats },
    samples: { ok: sampleEval.ok, stats: sampleEval.stats },
    failure_count: failures.length,
    failures: failures.slice(0, 50).map((f) => ({ ...f, detail: redactEvidence(f.detail) })),
    motion_per_window: motion.per_window,
  };
}

// Offline replay over a captured robot.log + soak-telemetry.json:
//   bun tests/pip-e2e/soak-gate.mjs --robot-log robot.log --telemetry soak-telemetry.json \
//     --start <iso|epoch_ms> --end <iso|epoch_ms> [--no-vision]
// Exits 1 when the gate fails. Prints redacted JSON only.
if (import.meta.main) {
  const fs = await import('node:fs');
  const args = process.argv.slice(2);
  const opt = (k) => {
    const i = args.indexOf(`--${k}`);
    return i >= 0 ? args[i + 1] : null;
  };
  const toMs = (v) => (v == null ? NaN : /^\d+$/.test(v) ? Number(v) : Date.parse(v));
  const robotLog = fs.readFileSync(opt('robot-log'), 'utf8');
  const samples = opt('telemetry') ? JSON.parse(fs.readFileSync(opt('telemetry'), 'utf8')) : [];
  const result = evaluateSoakGate({
    robotLog,
    samples,
    startMs: toMs(opt('start')),
    endMs: toMs(opt('end')),
    includeVision: !args.includes('--no-vision'),
  });
  const { motion_per_window, ...summary } = result;
  console.log(JSON.stringify(summary, null, 2));
  process.exit(result.ok ? 0 : 1);
}
