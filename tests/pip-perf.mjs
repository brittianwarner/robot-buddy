#!/usr/bin/env bun
// Offline functional tests for scripts/pip-perf.py, scripts/pip-perf-whisper-shim.sh,
// and scripts/pip-perf-warm-whisper-exec.sh.
// Exercises acoustic WAV analysis (mono + stereo EoS->first audible and wake END->playback stop),
// journal parsing (late cycles vs skipped 20 ms slots, positive camera inference evidence,
// turn cancellations), cgroup snapshot parsing, qualification & provisional acceptance gates
// (neural vs transcript wake, effective MemoryMax <= 1 GiB, kernel memory.peak vs polled lower
// bound, refusal of allow_inactive/simulated/unqualified runs), warm-whisper shim, and wrapper.
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const root = path.resolve(import.meta.dir, '..');
const perfScript = path.join(root, 'scripts/pip-perf.py');
const shimScript = path.join(root, 'scripts/pip-perf-whisper-shim.sh');
const execWrapper = path.join(root, 'scripts/pip-perf-warm-whisper-exec.sh');

const work = fs.mkdtempSync(path.join(os.tmpdir(), 'pip-perf-test-'));

function runPerf(args, { expectCode = 0, input } = {}) {
  const res = spawnSync('python3', [perfScript, ...args], {
    cwd: root,
    encoding: 'utf8',
    input,
  });
  assert.equal(
    res.status,
    expectCode,
    `pip-perf ${args.join(' ')} exited ${res.status} (expected ${expectCode})\nstderr: ${res.stderr}\nstdout: ${res.stdout}`,
  );
  return res;
}

/** Write a 16-bit signed little-endian PCM WAV from per-channel sample arrays. */
function writeWav(filePath, rate, channelsData) {
  const channels = channelsData.length;
  const nframes = channelsData[0].length;
  const dataBytes = nframes * channels * 2;
  const buf = Buffer.alloc(44 + dataBytes);
  buf.write('RIFF', 0);
  buf.writeUInt32LE(36 + dataBytes, 4);
  buf.write('WAVEfmt ', 8);
  buf.writeUInt32LE(16, 16);
  buf.writeUInt16LE(1, 20);
  buf.writeUInt16LE(channels, 22);
  buf.writeUInt32LE(rate, 24);
  buf.writeUInt32LE(rate * channels * 2, 28);
  buf.writeUInt16LE(channels * 2, 32);
  buf.writeUInt16LE(16, 34);
  buf.write('data', 36);
  buf.writeUInt32LE(dataBytes, 40);
  let off = 44;
  for (let i = 0; i < nframes; i++) {
    for (let c = 0; c < channels; c++) {
      const clamped = Math.max(-32768, Math.min(32767, Math.round(channelsData[c][i])));
      buf.writeInt16LE(clamped, off);
      off += 2;
    }
  }
  fs.writeFileSync(filePath, buf);
  return buf;
}

function makeTrack(rate, totalMs, segments, noiseAmp = 25) {
  const total = Math.round((rate * totalMs) / 1000);
  const out = new Int16Array(total);
  for (let i = 0; i < total; i++) {
    out[i] = Math.round(noiseAmp * Math.sin((2 * Math.PI * 60 * i) / rate));
  }
  for (const { startMs, endMs, amp, freq } of segments) {
    const a = Math.round((rate * startMs) / 1000);
    const b = Math.min(total, Math.round((rate * endMs) / 1000));
    for (let i = a; i < b; i++) {
      out[i] += Math.round(amp * Math.sin((2 * Math.PI * freq * i) / rate));
    }
  }
  return out;
}

try {
  const rate = 16000;

  // 1. Mono EoS -> first audible (prompt 400..1400 ms, reply 2750..3600 ms => 1350 ms).
  {
    const wav = path.join(work, 'turn-mono.wav');
    const track = makeTrack(rate, 4000, [
      { startMs: 400, endMs: 850, amp: 5000, freq: 440 },
      { startMs: 950, endMs: 1400, amp: 5000, freq: 520 },
      { startMs: 2750, endMs: 3600, amp: 6000, freq: 330 },
    ]);
    writeWav(wav, rate, [track]);
    const res = JSON.parse(runPerf(['analyze-turn', wav]).stdout);
    assert.equal(res.eos_source, 'mono_auto');
    assert.equal(res.eos_ms, 1400);
    assert.equal(res.first_audible_ms, 2750);
    assert.equal(res.eos_to_first_audible_ms, 1350);
    assert.ok(res.reply_snr_db > 30);
  }

  // 2. Stereo EoS -> first audible (ch1 stimulus 300..1200 ms, ch0 reply at 2100 ms => 900 ms).
  {
    const wav = path.join(work, 'turn-stereo.wav');
    const mic = makeTrack(rate, 3500, [{ startMs: 2100, endMs: 3100, amp: 5500, freq: 380 }]);
    const stim = makeTrack(rate, 3500, [{ startMs: 300, endMs: 1200, amp: 7000, freq: 440 }], 5);
    writeWav(wav, rate, [mic, stim]);
    const res = JSON.parse(runPerf(['analyze-turn', wav]).stdout);
    assert.equal(res.eos_source, 'stereo_stimulus_ch1');
    assert.equal(res.eos_ms, 1200);
    assert.equal(res.first_audible_ms, 2100);
    assert.equal(res.eos_to_first_audible_ms, 900);
  }

  // 3. Mono wake END -> actual playback stop (Pip playing 0..2420 ms, wake_end=2100 ms => 320 ms).
  {
    const wav = path.join(work, 'barge-mono.wav');
    const track = makeTrack(rate, 3600, [
      { startMs: 200, endMs: 1100, amp: 5500, freq: 300 },
      { startMs: 1180, endMs: 2420, amp: 5500, freq: 310 },
    ]);
    writeWav(wav, rate, [track]);
    const res = JSON.parse(runPerf(['analyze-barge', wav, '--wake-end-ms', '2100']).stdout);
    assert.equal(res.wake_source, 'explicit_wake_end_ms');
    assert.equal(res.playback_was_audible, true);
    assert.equal(res.playback_stop_ms, 2420);
    assert.equal(res.wake_to_playback_stop_ms, 320);
  }

  // 4. Stereo wake -> actual playback stop (neural wake stops before wake phrase finishes).
  {
    const wav = path.join(work, 'barge-stereo.wav');
    const mic = makeTrack(rate, 3200, [{ startMs: 100, endMs: 1750, amp: 5000, freq: 340 }]);
    const stim = makeTrack(rate, 3200, [{ startMs: 1500, endMs: 2000, amp: 6500, freq: 500 }], 5);
    writeWav(wav, rate, [mic, stim]);
    const res = JSON.parse(runPerf(['analyze-barge', wav]).stdout);
    assert.equal(res.wake_source, 'stereo_stimulus_ch1');
    assert.equal(res.wake_onset_ms, 1500);
    assert.equal(res.wake_end_ms, 2000);
    assert.equal(res.playback_stop_ms, 1750);
    assert.equal(res.wake_onset_to_playback_stop_ms, 250);
    assert.equal(res.wake_to_playback_stop_ms, -250);
  }

  // 5. Journal parser distinguishes late_cycles vs skipped_slots and requires positive vision_frames.
  {
    const journalSample = [
      '\x1b[2m2026-11-24T17:10:05.001Z\x1b[0m \x1b[32m INFO\x1b[0m robot_buddy_robot::motion: neck loop timing period_ms=20 ticks=250 missed=0 skipped_slots=0 total_ticks=250 total_missed=0 total_skipped_slots=0 p95_cycle_us=1840 max_cycle_us=2910 max_work_us=1620 max_overrun_us=0',
      '2026-11-24T17:10:05.100Z  INFO robot_buddy_robot::vision: camera inference active vision_frames=10',
      '2026-11-24T17:10:08.412Z DEBUG robot_buddy_robot::audio: speech first audio headers_ms=340 first_pcm_ms=345',
      '2026-11-24T17:10:10.004Z  WARN robot_buddy_robot::motion: neck loop missed 20ms deadline period_ms=20 ticks=250 missed=2 skipped_slots=5 total_ticks=500 total_missed=2 total_skipped_slots=5 p95_cycle_us=3120 max_cycle_us=64800 max_work_us=64100 max_overrun_us=44800',
      '2026-11-24T17:10:10.105Z  INFO robot_buddy_robot::vision: camera inference active vision_frames=85',
      'event sequence=9 turn=abc kind=turn.cancelled',
      '2026-11-24T17:10:12.110Z DEBUG robot_buddy_robot::audio: speech first audio headers_ms=410 first_pcm_ms=420',
    ].join('\n');
    const parsed = JSON.parse(runPerf(['parse-journal', '-'], { input: journalSample }).stdout);
    assert.equal(parsed.motion_20ms.windows, 2);
    assert.equal(parsed.motion_20ms.total_ticks, 500);
    assert.equal(parsed.motion_20ms.late_cycles, 2);
    assert.equal(parsed.motion_20ms.skipped_slots, 5);
    assert.equal(parsed.motion_20ms.late_cycle_rate_pct, 0.4);
    assert.equal(parsed.motion_20ms.skipped_slot_rate_pct, 1.0);
    assert.equal(parsed.motion_20ms.neck_active, true);
    assert.equal(parsed.vision.activity_events, 2);
    assert.equal(parsed.vision.vision_frames_in_window, 75);
    assert.equal(parsed.vision.camera_active, true);
    assert.equal(parsed.health.safety_fault_events, 0);
    assert.equal(parsed.health.turn_cancelled_events, 1);
    assert.equal(parsed.audio_journal.first_pcm_ms.p95, 420);

    // Zero vision lines (even with zero camera errors!) must NOT count as camera_active.
    const noVision = JSON.parse(
      runPerf(['parse-journal', '-'], {
        input:
          '2026-11-24T17:10:05Z INFO robot_buddy_robot::motion: neck loop timing period_ms=20 ticks=250 missed=0 skipped_slots=0 total_ticks=250 total_missed=0 total_skipped_slots=0 p95_cycle_us=1200 max_cycle_us=1900 max_work_us=1000 max_overrun_us=0\n',
      }).stdout,
    );
    assert.equal(noVision.vision.camera_active, false);
    assert.equal(noVision.vision.vision_frames_in_window, 0);

    // Stalled vision counter (same cumulative value repeated) is also inactive.
    const stalledVision = JSON.parse(
      runPerf(['parse-journal', '-'], {
        input: [
          'INFO robot_buddy_robot::vision: camera inference active vision_frames=42',
          'INFO robot_buddy_robot::vision: camera inference active vision_frames=42',
        ].join('\n'),
      }).stdout,
    );
    assert.equal(stalledVision.vision.camera_active, false);
    assert.equal(stalledVision.vision.vision_frames_in_window, 0);
  }

  // 6. Cgroup snapshot parser.
  {
    const showFile = path.join(work, 'systemctl.txt');
    const eventsFile = path.join(work, 'memory.events');
    fs.writeFileSync(
      showFile,
      [
        'ActiveState=active',
        'SubState=running',
        'MainPID=1234',
        'ControlGroup=/system.slice/robot-buddy.service',
        'MemoryCurrent=419430400',
        'MemoryPeak=629145600',
        'MemoryMax=1073741824',
        'TasksCurrent=28',
        'TasksMax=256',
        'NRestarts=0',
        'Result=success',
      ].join('\n'),
    );
    fs.writeFileSync(eventsFile, 'low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\n');
    const snap = JSON.parse(
      runPerf(['cgroup-snapshot', '--systemctl-file', showFile, '--events-file', eventsFile]).stdout,
    );
    assert.equal(snap.active_state, 'active');
    assert.equal(snap.memory_peak_bytes, 629145600);
    assert.equal(snap.memory_max_bytes, 1073741824);
    assert.equal(snap.memory_events.oom_kill, 0);
  }

  // 7. Three-variant comparison + strict qualification & provisional acceptance thresholds.
  {
    const GIB = 1024 * 1024 * 1024;
    const mkArtifact = (variant, eosP95, bargeP95, lateCycles, skippedSlots, kernelPeakMiB, overrides = {}) => ({
      schema_version: 2,
      variant,
      wake_mode: 'neural',
      allow_inactive: false,
      simulated: false,
      camera_active: true,
      neck_active: true,
      echo_interruptions: 0,
      metrics_ms: {
        eos_to_first_audible: { n: 20, p50: eosP95 - 120, p95: eosP95, p99: eosP95 + 40, max: eosP95 + 40 },
        wake_to_playback_stop: { n: 10, p50: bargeP95 - 40, p95: bargeP95, p99: bargeP95, max: bargeP95 },
      },
      motion_20ms: {
        total_ticks: 6000,
        late_cycles: lateCycles,
        skipped_slots: skippedSlots,
        p95_cycle_us_max: 2400,
        max_overrun_us: skippedSlots > 0 ? 1500 : 0,
        neck_stopped_events: 0,
      },
      vision: {
        activity_events: 24,
        vision_frames_in_window: 1400,
        camera_active: true,
        camera_error_events: 0,
      },
      cgroup_memory: {
        memory_limit_bytes: GIB,
        cgroup_memory_max_bytes: GIB,
        kernel_peak_bytes: Math.round(kernelPeakMiB * 1024 * 1024),
        polled_peak_lower_bound_bytes: Math.round((kernelPeakMiB - 30) * 1024 * 1024),
        restarts_delta: 0,
        oom_kills: 0,
      },
      health: {
        camera_error_events: 0,
        neck_stopped_events: 0,
        whisper_error_events: 0,
        safety_fault_events: 0,
        turn_cancelled_events: 0,
      },
      failures: [],
      ...overrides,
    });

    const basePath = path.join(work, 'baseline.json');
    const warmPath = path.join(work, 'warm-whisper.json');
    const shortPath = path.join(work, 'endpoint-shortening.json');
    const transcriptPath = path.join(work, 'transcript-baseline.json');
    fs.writeFileSync(basePath, JSON.stringify(mkArtifact('baseline', 2100, 420, 0, 0, 520)));
    fs.writeFileSync(warmPath, JSON.stringify(mkArtifact('warm-whisper', 1650, 390, 2, 3, 790)));
    fs.writeFileSync(shortPath, JSON.stringify(mkArtifact('endpoint-shortening', 1750, 410, 0, 0, 525)));
    fs.writeFileSync(
      transcriptPath,
      JSON.stringify(
        mkArtifact('baseline', 2100, null, 0, 0, 510, {
          wake_mode: 'transcript',
          metrics_ms: {
            eos_to_first_audible: { n: 20, p50: 1980, p95: 2100, p99: 2140, max: 2140 },
            wake_to_playback_stop: { n: 0, applicable: false },
          },
        }),
      ),
    );

    const md = runPerf(['compare', '--strict', basePath, warmPath, shortPath]).stdout;
    assert.ok(md.includes('Provisional acceptance targets (not measured hardware guarantees)'));
    assert.ok(md.includes('`baseline`') && md.includes('`warm-whisper`') && md.includes('`endpoint-shortening`'));
    assert.ok(md.includes('-450.0 ms'));
    assert.ok(md.includes('-350.0 ms'));
    assert.ok(md.includes('2 late / 3 skipped'));
    assert.ok(md.includes('520.0 MiB (lb 490.0 MiB) / 1024.0 MiB'));
    assert.ok(!md.includes('FAIL') && !md.includes('UNQUALIFIED'));

    // Transcript mode passes with 0 barge-in samples, marks N/A, and reports echo immunity.
    const tMd = runPerf(['compare', '--strict', transcriptPath]).stdout;
    assert.ok(tMd.includes('N/A (transcript; echo_int=0)'));
    assert.ok(!tMd.includes('FAIL') && !tMd.includes('UNQUALIFIED'));

    // Every individual failure/unqualified condition must fail --strict.
    const badCases = [
      ['allow-inactive', mkArtifact('baseline', 2100, 420, 0, 0, 520, { allow_inactive: true }), 'allow-inactive'],
      ['simulated', mkArtifact('baseline', 2100, 420, 0, 0, 520, { simulated: true }), 'simulated'],
      [
        'no-camera-frames',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, {
          vision: { activity_events: 0, vision_frames_in_window: 0, camera_active: false, camera_error_events: 0 },
        }),
        'camera-frames=0',
      ],
      [
        'no-neck-ticks',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, {
          motion_20ms: { total_ticks: 0, late_cycles: 0, skipped_slots: 0, p95_cycle_us_max: 0, max_overrun_us: 0 },
        }),
        'neck-ticks=0',
      ],
      [
        'too-few-turns',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, {
          metrics_ms: {
            eos_to_first_audible: { n: 5, p50: 1900, p95: 2100, p99: 2100, max: 2100 },
            wake_to_playback_stop: { n: 10, p50: 380, p95: 420, p99: 420, max: 420 },
          },
        }),
        'turns<20(5)',
      ],
      [
        'transcript-has-barge-samples',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, { wake_mode: 'transcript' }),
        'transcript-has-barge-samples(10)',
      ],
      [
        'transcript-echo-interruption',
        mkArtifact('baseline', 2100, null, 0, 0, 510, {
          wake_mode: 'transcript',
          echo_interruptions: 1,
          metrics_ms: {
            eos_to_first_audible: { n: 20, p50: 1980, p95: 2100, p99: 2140, max: 2140 },
            wake_to_playback_stop: { n: 0, applicable: false },
          },
        }),
        'echo_interruptions=1',
      ],
      [
        'effective-cap-above-1gib',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, {
          cgroup_memory: {
            memory_limit_bytes: GIB,
            cgroup_memory_max_bytes: 2 * GIB,
            kernel_peak_bytes: 520 * 1024 * 1024,
            polled_peak_lower_bound_bytes: 490 * 1024 * 1024,
            restarts_delta: 0,
            oom_kills: 0,
          },
        }),
        'cgroup-limit>1GiB',
      ],
      [
        'kernel-peak-missing-polled-only',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, {
          cgroup_memory: {
            memory_limit_bytes: GIB,
            cgroup_memory_max_bytes: GIB,
            kernel_peak_bytes: null,
            polled_peak_lower_bound_bytes: 490 * 1024 * 1024,
            restarts_delta: 0,
            oom_kills: 0,
          },
        }),
        'cgroup-kernel-peak-unknown',
      ],
      ['eos-over-3s', mkArtifact('baseline', 3150, 420, 0, 0, 520), 'eos_p95>3000ms'],
      ['barge-over-500ms', mkArtifact('baseline', 2100, 540, 0, 0, 520), 'wake_stop_p95>500ms(neural)'],
      ['skipped-slots-over-0.1pct', mkArtifact('baseline', 2100, 420, 4, 8, 520), 'skipped_slots>0.1%'],
      ['breaches-10pct-reserve', mkArtifact('warm-whisper', 1650, 390, 0, 0, 950), 'memory>90%_reserve'],
      ['over-1gib', mkArtifact('warm-whisper', 1650, 390, 0, 0, 1080), 'memory>=1GiB'],
      [
        'safety-fault',
        mkArtifact('baseline', 2100, 420, 0, 0, 520, {
          health: { camera_error_events: 0, neck_stopped_events: 0, whisper_error_events: 0, safety_fault_events: 1 },
        }),
        'safety_faults=1',
      ],
    ];
    for (const [name, data, expectedTag] of badCases) {
      const badPath = path.join(work, `bad-${name}.json`);
      fs.writeFileSync(badPath, JSON.stringify(data));
      const res = runPerf(['compare', '--strict', badPath], { expectCode: 1 });
      assert.ok(
        res.stdout.includes(expectedTag),
        `case ${name}: expected "${expectedTag}" in output:\n${res.stdout}`,
      );
    }
  }

  // 8. Warm-whisper shim works under env_clear (PATH + HOME only), cleans temp WAV, rejects non-loopback.
  {
    const shimHome = path.join(work, 'shim-home');
    const shimTmp = path.join(work, 'shim-tmp');
    fs.mkdirSync(shimHome);
    fs.mkdirSync(shimTmp);
    let uploadedBytes = null;
    const server = Bun.serve({
      hostname: '127.0.0.1',
      port: 0,
      async fetch(req) {
        if (req.method !== 'POST' || new URL(req.url).pathname !== '/inference') {
          return new Response('not found', { status: 404 });
        }
        const form = await req.formData();
        assert.equal(form.get('response_format'), 'text');
        const file = form.get('file');
        assert.ok(file && typeof file.arrayBuffer === 'function');
        uploadedBytes = Buffer.from(await file.arrayBuffer());
        return new Response(' Hey Pip,\nwhat time is it?\n', {
          headers: { 'content-type': 'text/plain' },
        });
      },
    });
    const runShim = (args, { env, input }) =>
      new Promise((resolve, reject) => {
        const child = spawn(shimScript, args, { env, stdio: ['pipe', 'pipe', 'pipe'] });
        const stdout = [];
        const stderr = [];
        child.stdout.on('data', (c) => stdout.push(c));
        child.stderr.on('data', (c) => stderr.push(c));
        child.on('error', reject);
        child.on('close', (status) =>
          resolve({ status, stdout: Buffer.concat(stdout), stderr: Buffer.concat(stderr) }),
        );
        child.stdin.end(input);
      });
    try {
      fs.writeFileSync(path.join(shimHome, '.pip-perf-whisper-url'), `http://127.0.0.1:${server.port}/inference\n`);
      const sampleWav = writeWav(path.join(work, 'shim-input.wav'), 16000, [new Int16Array(1600)]);
      const res = await runShim(
        ['-m', '/opt/robot-buddy/models/ggml-tiny.en.bin', '-l', 'en', '-t', '2', '-np', '-nt', '-f', '-'],
        {
          env: { PATH: process.env.PATH, HOME: shimHome, TMPDIR: shimTmp },
          input: sampleWav,
        },
      );
      assert.equal(res.status, 0, `shim failed: ${res.stderr.toString()}`);
      assert.equal(res.stdout.toString().trim().replace(/\s+/g, ' '), 'Hey Pip, what time is it?');
      assert.ok(uploadedBytes && uploadedBytes.equals(sampleWav));
      assert.deepEqual(fs.readdirSync(shimTmp), []);

      fs.writeFileSync(path.join(shimHome, '.pip-perf-whisper-url'), 'https://example.invalid/inference\n');
      const refused = await runShim(['-m', 'x', '-f', '-'], {
        env: { PATH: process.env.PATH, HOME: shimHome, TMPDIR: shimTmp },
        input: sampleWav,
      });
      assert.equal(refused.status, 2);
      assert.deepEqual(fs.readdirSync(shimTmp), []);
    } finally {
      server.stop(true);
    }
  }

  // 9. Warm-whisper ExecStart wrapper starts whisper-server, runs robot, and stops whisper-server on exit.
  {
    const fakeServerPidFile = path.join(work, 'whisper.pid');
    const fakeWhisper = path.join(work, 'fake-whisper-server.sh');
    const fakeRobot = path.join(work, 'fake-robot.sh');
    const probe = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch: () => new Response('ok') });
    try {
      fs.writeFileSync(
        fakeWhisper,
        `#!/bin/sh\necho "$$" > "${fakeServerPidFile}"\nexec sleep 30\n`,
        { mode: 0o755 },
      );
      fs.writeFileSync(fakeRobot, '#!/bin/sh\nexit 0\n', { mode: 0o755 });
      const res = await new Promise((resolve, reject) => {
        const child = spawn(execWrapper, [], {
          env: {
            PATH: process.env.PATH,
            PIP_PERF_WHISPER_SERVER: fakeWhisper,
            PIP_PERF_WHISPER_PORT: String(probe.port),
            PIP_PERF_ROBOT_BIN: fakeRobot,
            BUDDY_LISTEN_MODEL: '/tmp/model.bin',
          },
          stdio: ['ignore', 'pipe', 'pipe'],
        });
        const stderr = [];
        child.stderr.on('data', (c) => stderr.push(c));
        child.on('error', reject);
        child.on('close', (status) => resolve({ status, stderr: Buffer.concat(stderr).toString() }));
      });
      assert.equal(res.status, 0, `wrapper failed: ${res.stderr}`);
      const wpid = Number(fs.readFileSync(fakeServerPidFile, 'utf8').trim());
      assert.ok(Number.isFinite(wpid) && wpid > 0);
      let alive = true;
      try {
        process.kill(wpid, 0);
      } catch {
        alive = false;
      }
      assert.equal(alive, false, `whisper-server pid ${wpid} must be terminated on wrapper exit`);
    } finally {
      probe.stop(true);
    }
  }

  console.error('pip-perf tests: all 9 suites (including 15 strict gate rejections) passed');
} finally {
  fs.rmSync(work, { recursive: true, force: true });
}
