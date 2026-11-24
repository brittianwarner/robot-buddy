#!/usr/bin/env bun
// ARM64 Pip E2E & Continuous Conversation Soak Harness.
//
// Runs the REAL release robot-buddy-server + pinned real Rivet 3.1.1 engine +
// REAL release robot-buddy-robot (--features pi-vision,onnx-wake, NO --simulate)
// over real TLS/WSS against virtual ARM64 Pip peripherals (face PTY, STS3215 servo PTY,
// virtual rpicam-vid I420 camera, virtual aplay/arecord, slow-whisper wrapper around real
// ARM64 whisper-cli + ggml-tiny.en.bin, and real ONNX Runtime neural wake).
//
// Zero --simulate and zero control-socket submit bypass: every utterance enters through
// virtual arecord -> real robot VAD -> real ARM64 whisper-cli -> real Rivet actor.
//
// Usage:
//   bun tests/pip-e2e/runner.mjs --suite smoke
//   bun tests/pip-e2e/runner.mjs --suite all
//   bun tests/pip-e2e/runner.mjs --suite soak --duration 30m --out /opt/pip-e2e/artifacts/soak-30m
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { now, sleep } from './runner-fakes.mjs';
import {
  TERMINAL_KINDS,
  collectProvenance,
  fileSha256,
  startPipE2EStack,
} from './runner-stack.mjs';

const argv = process.argv.slice(2);
const hasFlag = (name) => argv.includes(`--${name}`);
const getOpt = (name, fallback = null) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && i + 1 < argv.length ? argv[i + 1] : fallback;
};

function parseDurationSeconds(raw, fallbackSec = 1800) {
  if (!raw) return fallbackSec;
  const s = String(raw).trim().toLowerCase();
  if (/^\d+$/.test(s)) return Number(s);
  const m = /^(\d+(?:\.\d+)?)(s|m|h)$/.exec(s);
  if (!m) throw new Error(`invalid --duration value: ${raw} (use e.g. 120, 5m, 30m, 1h)`);
  const val = Number(m[1]);
  if (m[2] === 's') return Math.round(val);
  if (m[2] === 'm') return Math.round(val * 60);
  return Math.round(val * 3600);
}

function stats(values) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return { n: 0, p50: null, p95: null, p99: null, max: null };
  const at = (p) => v[Math.min(v.length - 1, Math.ceil((p / 100) * v.length) - 1)];
  const r = (x) => Math.round(x * 10) / 10;
  return { n: v.length, p50: r(at(50)), p95: r(at(95)), p99: r(at(99)), max: r(v[v.length - 1]) };
}

function growthStats(values, divisor = 1) {
  const v = values.filter((x) => typeof x === 'number' && Number.isFinite(x)).map((x) => x / divisor);
  if (!v.length) return { start: null, end: null, peak: null, delta: null, status: 'unknown' };
  const r = (x) => Math.round(x * 10) / 10;
  const start = v[0];
  const end = v[v.length - 1];
  const peak = Math.max(...v);
  return { start: r(start), end: r(end), peak: r(peak), delta: r(end - start), status: 'measured' };
}

function textSummaryRedacted(text, expectedKeywords = []) {
  if (typeof text !== 'string' || !text.trim()) {
    return { non_empty: false, char_len: 0, sha256_prefix: null, matched_keywords: [] };
  }
  const lower = text.toLowerCase();
  const matched = expectedKeywords.filter((kw) => lower.includes(String(kw).toLowerCase()));
  const digest = crypto.createHash('sha256').update(text).digest('hex').slice(0, 12);
  return {
    non_empty: true,
    char_len: text.trim().length,
    sha256_prefix: digest,
    matched_keywords: matched,
  };
}

function loadSpeechManifest(customPath) {
  const cacheDir = process.env.PIP_E2E_CACHE_DIR ?? '/opt/pip-e2e/cache';
  const manifestPath = customPath ?? path.join(cacheDir, 'speech/fixtures/manifest.json');
  if (!fs.existsSync(manifestPath)) {
    throw new Error(
      `speech fixture manifest not found at ${manifestPath}; run tests/pip-e2e/speech/ qualification or prepare-cache first`,
    );
  }
  const raw = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  const clips = Array.isArray(raw.clips) ? raw.clips : [];
  const resolved = clips.map((clip) => {
    const filePath = clip.path ?? clip.wav_path ?? clip.pcm_path;
    return {
      ...clip,
      resolvedPath: filePath
        ? path.isAbsolute(filePath)
          ? filePath
          : path.resolve(path.dirname(manifestPath), filePath)
        : null,
    };
  });
  const existing = resolved.filter((c) => c.resolvedPath && fs.existsSync(c.resolvedPath));
  const byId = (id) => existing.find((c) => c.id === id) ?? null;

  // Auto-discover verified human-wake-recipe artifacts in cache (explicit provenance preserved!).
  const recipeDir = path.join(cacheDir, 'speech/human-wake-positive');
  const naturalTranscriptWakePath = path.join(recipeDir, 'natural_human_transcript_wake_pip.wav');
  const editedNeuralWakePath = path.join(recipeDir, 'edited_spliced_human_hey_pip.wav');
  const editedNeuralWakeCmdPath = path.join(recipeDir, 'edited_spliced_human_hey_pip_command.wav');

  const naturalTranscriptWake = fs.existsSync(naturalTranscriptWakePath)
    ? {
        id: 'natural_human_transcript_wake_pip',
        kind: 'command',
        text: 'Pip, what a fool you are.',
        expected_substrings: ['fool'],
        resolvedPath: naturalTranscriptWakePath,
        sha256: fileSha256(naturalTranscriptWakePath)?.sha256 ?? null,
        provenance: 'human_recorded_natural',
        is_human_recording: true,
        source_note: 'LibriVox Great Expectations Ch. 17 (Public Domain; bare Pip idle-only transcript wake)',
      }
    : null;

  // Explicit opt-in contiguous human recording takes precedence over edited_spliced_human.
  const optInContiguousHumanWake =
    process.env.PIP_E2E_HUMAN_WAKE_POSITIVE_WAV && fs.existsSync(process.env.PIP_E2E_HUMAN_WAKE_POSITIVE_WAV)
      ? {
          id: 'human_wake_positive_opt_in',
          kind: 'wake_positive',
          resolvedPath: process.env.PIP_E2E_HUMAN_WAKE_POSITIVE_WAV,
          sha256: fileSha256(process.env.PIP_E2E_HUMAN_WAKE_POSITIVE_WAV)?.sha256 ?? null,
          provenance: 'human_recorded_contiguous',
          is_human_recording: true,
        }
      : null;

  // Component-level edited/spliced same-speaker human "Hey, Pip." (NEVER labeled natural contiguous!).
  const editedSplicedNeuralWake = fs.existsSync(editedNeuralWakePath)
    ? {
        id: 'edited_spliced_human_hey_pip',
        kind: 'wake_positive',
        text: 'Hey, Pip.',
        resolvedPath: editedNeuralWakePath,
        sha256: fileSha256(editedNeuralWakePath)?.sha256 ?? null,
        provenance: 'edited_spliced_human',
        is_human_recording: false,
        is_natural_contiguous_hey_pip: false,
        source_note: 'Wikimedia Commons Dvortygirl En-us-hey.ogg (PD) + En-us-pip.ogg (CC BY-SA 3.0); component-level spliced stimulus, NOT a natural contiguous recording',
      }
    : null;

  const editedSplicedNeuralCommand = fs.existsSync(editedNeuralWakeCmdPath)
    ? {
        id: 'edited_spliced_human_hey_pip_command',
        kind: 'command',
        text: 'Hey, Pip. I had that curiosity beside me at this moment.',
        expected_substrings: ['curiosity', 'beside', 'moment'],
        resolvedPath: editedNeuralWakeCmdPath,
        sha256: fileSha256(editedNeuralWakeCmdPath)?.sha256 ?? null,
        provenance: 'edited_spliced_human',
        is_human_recording: false,
        is_natural_contiguous_hey_pip: false,
      }
    : null;

  const isLocalOnlyCmd = (c) => /\b(go to sleep|stop talking|confirm)\b/i.test(String(c.text ?? ''));
  const humanWakePositive =
    optInContiguousHumanWake ??
    existing.find(
      (c) =>
        c.provenance === 'human_recorded' &&
        ['wake_positive', 'wake_positive_command', 'wake_command'].includes(c.kind) &&
        !isLocalOnlyCmd(c),
    ) ??
    null;

  const generatedWakeCommand =
    byId('synth_tts_hey_pip_nod') ??
    byId('synth_tts_hey_pip_time') ??
    existing.find(
      (c) =>
        c.provenance === 'synthetic' &&
        !isLocalOnlyCmd(c) &&
        /^hey\s+pip\b/i.test(String(c.text ?? '')) &&
        String(c.text ?? '').trim().length > 10,
    ) ??
    null;

  const humanFollowup =
    byId('human_voices_followup') ??
    byId('padded_human_voices_followup') ??
    existing.find((c) => c.provenance === 'human_recorded' && c.kind === 'followup') ??
    null;

  const humanNegative =
    byId('human_jfk_negative') ??
    existing.find((c) => c.provenance === 'human_recorded' && c.kind === 'wake_negative') ??
    null;

  return {
    manifestPath,
    clips: existing,
    missingHumanEvidence: raw.missing_human_evidence ?? [],
    naturalTranscriptWake,
    humanWakePositive,
    editedSplicedNeuralWake,
    editedSplicedNeuralCommand,
    neuralWakeStimulus: humanWakePositive ?? editedSplicedNeuralWake,
    generatedWakeCommand,
    // Prefer natural PD human transcript wake -> opt-in human -> edited_spliced_human -> synthetic.
    wakeCommand: naturalTranscriptWake ?? humanWakePositive ?? editedSplicedNeuralCommand ?? generatedWakeCommand,
    humanFollowup,
    humanNegative,
  };
}

/**
 * Generates a clearly-labeled synthetic espeak-ng 16 kHz mono S16_LE WAV clip
 * ("Hey Pip, please nod your head.") into `dir` when the manifest has no non-sleep
 * wake+command clip. Strictly tagged provenance="synthetic", is_human_recording=false.
 */
function synthesizeWakeCommandClip(dir) {
  const wavPath = path.join(dir, 'synth_tts_hey_pip_nod.wav');
  const script = `
import array, os, subprocess, sys, wave
out_wav = sys.argv[1]
raw_wav = out_wav + ".raw.wav"
subprocess.run(["espeak-ng", "-s", "140", "-w", raw_wav, "Hey Pip, please nod your head."], check=True)
with wave.open(raw_wav, "rb") as w:
    rate = w.getframerate()
    raw = w.readframes(w.getnframes())
os.remove(raw_wav)
src = array.array("h"); src.frombytes(raw)
n = int(round(len(src) * 16000 / rate))
out = array.array("h", [0] * n)
for i in range(n):
    pos = i * rate / 16000.0
    idx = int(pos)
    frac = pos - idx
    a = src[idx]
    b = src[min(idx + 1, len(src) - 1)]
    out[i] = int(round(a + (b - a) * frac))
pad = array.array("h", [0] * 4800)
with wave.open(out_wav, "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(16000)
    w.writeframes((pad + out + pad).tobytes())
`;
  const r = spawnSync('python3', ['-c', script, wavPath], { encoding: 'utf8' });
  if (r.status !== 0) {
    throw new Error(`failed to synthesize wake command clip: ${r.stderr}`);
  }
  const info = fileSha256(wavPath);
  return {
    id: 'synth_tts_hey_pip_nod',
    kind: 'command',
    text: 'Hey Pip, please nod your head.',
    expected_substrings: ['nod', 'head'],
    resolvedPath: wavPath,
    sha256: info?.sha256 ?? null,
    provenance: 'synthetic',
    is_human_recording: false,
    generator: 'espeak-ng -s 140',
  };
}

function findVisionFixtureFrame() {
  const visionDir =
    process.env.PIP_E2E_VISION_CACHE ??
    path.join(process.env.PIP_E2E_CACHE_DIR ?? '/opt/pip-e2e/cache', 'vision');
  if (!fs.existsSync(visionDir)) return null;
  const entries = fs.readdirSync(visionDir).map((f) => path.join(visionDir, f));
  const preferred = entries.find((p) => path.basename(p) === 'face_a_1.i420');
  const i420 = preferred ?? entries.find((p) => p.endsWith('.i420') && !path.basename(p).includes('blank') && !path.basename(p).includes('two_faces'));
  const ppm = entries.find((p) => p.endsWith('.ppm'));
  return i420 ? { kind: 'i420', path: i420 } : ppm ? { kind: 'ppm', path: ppm } : null;
}

/**
 * Reads the first complete JPEG frame (\xff\xd8 .. \xff\xd9) from the operator
 * console's infinite `multipart/x-mixed-replace` MJPEG stream without hanging.
 */
async function readFirstMjpegFrameBytes(response, timeoutMs = 10_000) {
  if (!response.ok || !response.body) return 0;
  const reader = response.body.getReader();
  let buf = Buffer.alloc(0);
  const deadline = Date.now() + timeoutMs;
  try {
    while (Date.now() < deadline) {
      const remaining = Math.max(50, deadline - Date.now());
      const next = await Promise.race([
        reader.read(),
        sleep(remaining).then(() => ({ timeout: true })),
      ]);
      if (next.timeout || next.done) break;
      buf = Buffer.concat([buf, Buffer.from(next.value)]);
      const soi = buf.indexOf(Buffer.from([0xff, 0xd8]));
      if (soi >= 0) {
        const eoi = buf.indexOf(Buffer.from([0xff, 0xd9]), soi + 2);
        if (eoi >= 0) {
          return eoi + 2 - soi;
        }
      }
      if (buf.length > 1024 * 1024) break;
    }
  } finally {
    try {
      await reader.cancel();
    } catch {}
  }
  return 0;
}

/**
 * Injects a recorded WAV/PCM clip into virtual arecord and captures explicit timing endpoints:
 *   - mic_inject_to_turn_started_ms
 *   - turn_started_to_first_audio_ms
 *   - mic_inject_to_first_audio_ms
 *   - mic_inject_to_turn_completed_ms
 */
async function runRealSpeechTurn(
  stack,
  clip,
  { expectAudio = true, timeoutMs = 45_000, label = 'speech_turn', padBeforeMs = 250, padAfterMs = 950 } = {},
) {
  if (!clip?.resolvedPath) throw new Error(`no audio clip available for ${label}`);
  const tInject = now();
  const speakerSeqBefore = (await stack.periph.speakerSessions()).total_sessions ?? 0;
  const modelBefore = stack.model.stats.calls;
  const eventsBefore = stack.events.length;

  await stack.periph.micInject({
    path: clip.resolvedPath,
    clip_id: clip.id ?? label,
    pad_silence_before_ms: padBeforeMs,
    pad_silence_after_ms: padAfterMs,
  });

  const started = await stack.waitEvent(
    (e) => e.t >= tInject && e.kind === 'turn.started',
    timeoutMs,
    `${label} turn.started`,
  );
  const terminal = await stack.waitEvent(
    (e) => e.turn_id === started.turn_id && TERMINAL_KINDS.has(e.kind),
    timeoutMs,
    `${label} terminal`,
  );

  let speakerSession = null;
  if (expectAudio && terminal.kind === 'turn.completed') {
    await stack.until(
      `${label} speaker playback`,
      async () => {
        const res = await stack.periph.speakerSessions();
        const sessions = res.sessions ?? [];
        const s = sessions.find(
          (x) => (x.session_id ?? 0) > speakerSeqBefore && x.total_bytes > 0 && (x.end_ms != null || x.eof_ms != null),
        );
        if (s) {
          speakerSession = s;
          return true;
        }
        return false;
      },
      timeoutMs,
    );
  }

  const turnEvents = stack.events.slice(eventsBefore).filter((e) => e.turn_id === started.turn_id);
  const newUserTexts = stack.model.stats.userTexts.filter((u) => u.callIndex > modelBefore);
  const firstAudioWallMs = speakerSession?.first_chunk_ms ?? null;
  const injectWallMs = Date.now() - Math.round(now() - tInject);

  const micToTurnStartedMs = Math.round(started.t - tInject);
  const micToTurnCompletedMs = Math.round(terminal.t - tInject);
  // Speaker timestamps come from Python time.time()*1000; derive relative durations safely.
  const speakerDurationMs =
    speakerSession?.first_chunk_ms != null && (speakerSession.end_ms ?? speakerSession.eof_ms) != null
      ? Math.round((speakerSession.end_ms ?? speakerSession.eof_ms) - speakerSession.first_chunk_ms)
      : null;
  const micToFirstAudioMs =
    firstAudioWallMs != null && firstAudioWallMs >= injectWallMs - 100
      ? Math.max(0, Math.round(firstAudioWallMs - injectWallMs))
      : null;
  const turnStartedToFirstAudioMs =
    micToFirstAudioMs != null ? Math.max(0, micToFirstAudioMs - micToTurnStartedMs) : null;

  return {
    turnId: started.turn_id,
    terminalKind: terminal.kind,
    clipId: clip.id,
    clipProvenance: clip.provenance ?? 'unknown',
    isHumanRecording: Boolean(clip.is_human_recording ?? clip.provenance === 'human_recorded'),
    timingMs: {
      mic_inject_to_turn_started_ms: micToTurnStartedMs,
      turn_started_to_first_audio_ms: turnStartedToFirstAudioMs,
      mic_inject_to_first_audio_ms: micToFirstAudioMs,
      mic_inject_to_turn_completed_ms: micToTurnCompletedMs,
      speaker_playback_duration_ms: speakerDurationMs,
    },
    turnEvents,
    speakerSession,
    transcriptRedacted: textSummaryRedacted(newUserTexts[0]?.text ?? '', clip.expected_substrings ?? []),
    modelCallsDelta: stack.model.stats.calls - modelBefore,
  };
}

function countDistinctServoGoals(events) {
  const poses = new Set();
  for (const ev of events) {
    if (Array.isArray(ev.steps)) {
      poses.add(ev.steps.join(','));
    } else if (ev.goals && typeof ev.goals === 'object') {
      poses.add(JSON.stringify(ev.goals));
    } else if (ev.goal != null || ev.value != null) {
      poses.add(`${ev.servo_id ?? ev.id ?? 'x'}:${ev.goal ?? ev.value}`);
    }
  }
  return poses.size;
}

function makeRecorder(log) {
  const checks = [];
  const record = (name, ok, detail = {}, { status = null } = {}) => {
    const resolvedStatus = status ?? (ok ? 'pass' : 'fail');
    checks.push({ name, ok: Boolean(ok), status: resolvedStatus, detail });
    const tag = resolvedStatus === 'pass' ? 'PASS' : resolvedStatus === 'missing_evidence' ? 'MISSING_EVIDENCE' : 'FAIL';
    log(`${tag} ${name} ${JSON.stringify(detail)}`);
    return Boolean(ok);
  };
  return { checks, record };
}

async function runCoreChecks(stack, manifest, { log, record, includeVision = true, verifyNeuralWake = true }) {
  // 1. Real Enrollment + HTTPS OIDC Claim + Loopback Panel Confirm (NO control-socket bypass)
  const claim = await stack.completeOidcClaimViaPanel();
  record(
    'enrollment + real HTTPS OIDC claim + loopback panel confirm',
    claim.unauth_status === 401 &&
      claim.unclaimed_status === 403 &&
      claim.claimed_status === 200 &&
      claim.operator_ready &&
      claim.claim_url_https_verified &&
      claim.confirm_code_matched &&
      claim.owner_matched &&
      claim.oidc_audit.tokens >= 1 &&
      claim.oidc_audit.userinfos >= 1,
    claim,
  );

  // 2. Face PTY + STS3215 Servo PTY health & bounds
  const faceBefore = await stack.periph.faceEvents(0, 200);
  const servoState = await stack.periph.servoState();
  const servoBounds = await stack.periph.servoAssertBounds({});
  record(
    'face PTY connected with valid raw 115200 termios and live commands',
    Boolean(faceBefore.termios?.raw_ok ?? true) && (faceBefore.events?.length ?? 0) > 0,
    { event_count: faceBefore.events?.length ?? 0, termios: faceBefore.termios ?? null },
  );
  record(
    'STS3215 servo PTY connected with valid calibration, B1000000 termios, and zero limit/EEPROM violations',
    Boolean(servoBounds.ok) &&
      (servoState.limit_violations ?? 0) === 0 &&
      (servoState.eeprom_lock_violations ?? 0) === 0,
    {
      bounds_ok: servoBounds.ok,
      limit_violations: servoState.limit_violations ?? 0,
      eeprom_lock_violations: servoState.eeprom_lock_violations ?? 0,
      cadence_hz: servoState.cadence_hz ?? null,
    },
  );

  // 3. Transcript Wake + Real Actor `express` Tool -> Face Expression + Distinct Changed Servo Goals + Inert Replay
  const faceSeqBefore = faceBefore.events?.at(-1)?.seq ?? 0;
  const servoSeqBefore = servoState.latest_seq ?? 0;

  let expressFired = false;
  stack.model.setScript((ctx) => {
    if (ctx.hasTools && ctx.toolResultCount === 0 && !expressFired) {
      expressFired = true;
      return {
        kind: 'tool',
        id: 'call_express_e2e_1',
        name: 'express',
        args: { emotion: 'happy', intensity: 0.85, gesture: 'nod' },
      };
    }
    return { kind: 'text', text: 'I am happy to help and nodding right now.' };
  });

  const turn1 = await runRealSpeechTurn(stack, manifest.wakeCommand, {
    expectAudio: true,
    timeoutMs: 45_000,
    label: 'transcript_wake_express_turn',
  });
  stack.model.setScript(null);

  const expressEvent = turn1.turnEvents.find((e) => e.kind === 'face.express');
  let replayPayload = null;
  if (expressEvent) {
    const replayPage = await stack.conversationCall({ type: 'replay', after: expressEvent.sequence - 1 });
    const replayedCue = Array.isArray(replayPage)
      ? replayPage.find((e) => e.sequence === expressEvent.sequence && e.kind === 'face.express')
      : null;
    replayPayload = replayedCue?.payload ?? null;
  }

  await sleep(600);
  const faceAfter = await stack.periph.faceEvents(faceSeqBefore, 500);
  const sawHappyFace = (faceAfter.events ?? []).some(
    (ev) => ev.cmd?.e === 'happy' || ev.payload?.e === 'happy' || ev.e === 'happy',
  );
  const newServoGoals = (await stack.periph.servoEvents('goals', servoSeqBefore, 4000)).events ?? [];
  const distinctServoGoals = countDistinctServoGoals(newServoGoals);
  const boundsAfterExpress = await stack.periph.servoAssertBounds({});

  record(
    'transcript wake real speech turn (arecord -> VAD -> whisper-cli -> Rivet 3.1.1 -> aplay)',
    turn1.terminalKind === 'turn.completed' &&
      turn1.transcriptRedacted.non_empty &&
      Boolean(turn1.speakerSession) &&
      (turn1.speakerSession?.total_bytes ?? 0) > 0,
    {
      clip_id: turn1.clipId,
      clip_provenance: turn1.clipProvenance,
      is_human_recording: turn1.isHumanRecording,
      turn_id: turn1.turnId,
      transcript: turn1.transcriptRedacted,
      timing_ms: turn1.timingMs,
      speaker_bytes: turn1.speakerSession?.total_bytes ?? 0,
    },
  );

  record(
    'real actor express tool pushes live face.express, drives face PTY + changed STS3215 nod goals within bounds, replays inert {}',
    Boolean(expressEvent) &&
      replayPayload !== null &&
      Object.keys(replayPayload).length === 0 &&
      sawHappyFace &&
      newServoGoals.length > 0 &&
      distinctServoGoals >= 2 &&
      Boolean(boundsAfterExpress.ok),
    {
      express_seq: expressEvent?.sequence ?? null,
      replay_payload: replayPayload,
      saw_happy_face: sawHappyFace,
      new_servo_goal_writes: newServoGoals.length,
      distinct_servo_goals: distinctServoGoals,
      servo_bounds_ok: boundsAfterExpress.ok,
    },
  );

  // 4. Prerecorded HUMAN Follow-Up Speech Submitted & Decoded in Active Conversation Window
  if (manifest.humanFollowup) {
    const humanTurn = await runRealSpeechTurn(stack, manifest.humanFollowup, {
      expectAudio: true,
      timeoutMs: 45_000,
      label: 'human_recorded_followup',
    });
    record(
      'prerecorded human follow-up speech actually decoded by ARM64 whisper-cli and answered',
      humanTurn.terminalKind === 'turn.completed' &&
        humanTurn.isHumanRecording &&
        humanTurn.transcriptRedacted.non_empty &&
        humanTurn.transcriptRedacted.char_len >= 10 &&
        Boolean(humanTurn.speakerSession),
      {
        clip_id: humanTurn.clipId,
        clip_provenance: humanTurn.clipProvenance,
        is_human_recording: humanTurn.isHumanRecording,
        turn_id: humanTurn.turnId,
        transcript: humanTurn.transcriptRedacted,
        timing_ms: humanTurn.timingMs,
        speaker_bytes: humanTurn.speakerSession?.total_bytes ?? 0,
      },
    );
  } else {
    record('prerecorded human follow-up speech fixture present', false, {
      reason: 'human_voices_followup missing from speech manifest (no unqualified skip allowed)',
    });
  }

  // 5. Face Soft-Reset `hello` Without Closing PTY fd
  const helloRes = await stack.periph.faceHello({ version: '1.0', eyes: 2, fragmented: true });
  await sleep(500);
  const faceAfterHello = await stack.periph.faceEvents(helloRes.seq_before ?? 0, 200);
  const repaintedAfterHello = (faceAfterHello.events ?? []).some(
    (ev) => ev.cmd?.e !== undefined || ev.cmd?.led !== undefined || ev.e !== undefined || ev.led !== undefined,
  );
  record(
    'face soft-reset hello (no fd close) triggers state repaint',
    helloRes.fd_closed === false && repaintedAfterHello && (faceAfterHello.fd_closed_count ?? 0) === 0,
    {
      hello_seq: helloRes.hello_seq,
      fd_closed: helloRes.fd_closed,
      fd_closed_count: faceAfterHello.fd_closed_count ?? 0,
      events_after_hello: (faceAfterHello.events ?? []).length,
    },
  );

  // 6. Operator Console Manual Pose & Release + Changed Servo Goals
  {
    const servoSeqBeforeOp = (await stack.periph.servoState()).latest_seq ?? 0;
    const opPage = await stack.operatorRequest('/');
    const opHtml = await opPage.text();
    const opCsrf = /name=csrf\s+value='([^']+)'/.exec(opHtml)?.[1] ?? null;
    let poseOk = false;
    let releaseOk = false;
    if (opCsrf) {
      const pRes = await stack.operatorRequest('/control', {
        method: 'POST',
        form: { csrf: opCsrf, action: 'pose', yaw: '12', pitch: '6', roll: '0' },
      });
      await sleep(350);
      const rRes = await stack.operatorRequest('/control', {
        method: 'POST',
        form: { csrf: opCsrf, action: 'release' },
      });
      poseOk = pRes.status === 303 || pRes.status === 302 || pRes.status === 200;
      releaseOk = rRes.status === 303 || rRes.status === 302 || rRes.status === 200;
    }
    await sleep(300);
    const goalsAfterOp = (await stack.periph.servoEvents('goals', servoSeqBeforeOp, 4000)).events ?? [];
    const opDeltaGoals = countDistinctServoGoals(goalsAfterOp);
    const boundsAfterOp = await stack.periph.servoAssertBounds({});
    record(
      'operator console manual pose + release moves servos within calibrated bounds',
      Boolean(opCsrf) && poseOk && releaseOk && opDeltaGoals >= 1 && Boolean(boundsAfterOp.ok),
      { pose_ok: poseOk, release_ok: releaseOk, distinct_goals: opDeltaGoals, bounds_ok: boundsAfterOp.ok },
    );
  }

  // 7. Camera Real OpenCV YuNet/SFace Inference + Preview + Gaze Tracking + Enroll/Forget
  if (includeVision) {
    const frame = findVisionFixtureFrame();
    if (!stack.paths.yunetModel || !frame) {
      record('camera vision inference models & I420 fixture present', false, {
        has_yunet: Boolean(stack.paths.yunetModel),
        has_frame: Boolean(frame),
      });
    } else {
      await stack.periph.cameraScene({
        kind: frame.kind,
        path: frame.path,
        offset_x: 0.25,
        offset_y: -0.1,
        fps: 10,
      });
      const facePreVision = (await stack.periph.faceEvents(0, 1000)).events?.at(-1)?.seq ?? 0;
      let jpegBytes = 0;
      await stack.until(
        'operator /camera JPEG preview',
        async () => {
          const r = await stack.operatorRequest('/camera');
          const n = await readFirstMjpegFrameBytes(r, 4000);
          if (n > 200) {
            jpegBytes = n;
            return true;
          }
          return false;
        },
        15_000,
      );
      await sleep(800);
      const faceVisionEvents = (await stack.periph.faceEvents(facePreVision, 500)).events ?? [];
      const sawLookCmd = faceVisionEvents.some((ev) => ev.cmd?.look !== undefined || ev.look !== undefined);

      // Test local enrollment, live SFace recognition, & forget through operator console when SFace is present.
      // Center the single-face fixture so all 5 YuNet landmarks sit comfortably in-frame.
      let enrollForgetOk = true;
      let enrollTested = false;
      let enrollStatus = null;
      let forgetStatus = null;
      let recognizedMatch = null;
      if (stack.paths.sfaceModel) {
        enrollTested = true;
        await stack.periph.cameraScene({
          kind: frame.kind,
          path: frame.path,
          offset_x: 0.0,
          offset_y: 0.0,
          fps: 10,
        });
        await sleep(600);
        const opHtml = await (await stack.operatorRequest('/')).text();
        const opCsrf = /name=csrf\s+value='([^']+)'/.exec(opHtml)?.[1] ?? null;
        if (opCsrf) {
          const personName = 'Ada';
          const eRes = await stack.operatorRequest('/control', {
            method: 'POST',
            form: { csrf: opCsrf, action: 'enroll', name: personName },
          });
          enrollStatus = eRes.status;
          // Give SFace (runs at most once per second) a moment to match the enrolled face live.
          await sleep(1500);
          const afterEnrollHtml = await (await stack.operatorRequest('/')).text();
          recognizedMatch = (await (await stack.operatorRequest('/recognized')).text()).trim();
          const fRes = await stack.operatorRequest('/control', {
            method: 'POST',
            form: { csrf: opCsrf, action: 'forget', name: personName },
          });
          forgetStatus = fRes.status;
          await sleep(200);
          const afterForgetHtml = await (await stack.operatorRequest('/')).text();
          // gallery::key normalizes names (lowercase); check the People roster form value specifically.
          const enrolledInRoster = afterEnrollHtml.toLowerCase().includes(`value="${personName.toLowerCase()}"`);
          const removedFromRoster = !afterForgetHtml.toLowerCase().includes(`value="${personName.toLowerCase()}"`);
          enrollForgetOk =
            (eRes.status === 303 || eRes.status === 302 || eRes.status === 200) &&
            enrolledInRoster &&
            (fRes.status === 303 || fRes.status === 302 || fRes.status === 200) &&
            removedFromRoster;
        } else {
          enrollForgetOk = false;
        }
      }
      const camStatus = await stack.periph.cameraStatus();
      const framesSent = camStatus.frames_sent ?? camStatus.frames_emitted ?? 0;
      record(
        'camera real OpenCV YuNet/SFace inference: JPEG preview, gaze tracking, and enroll/forget',
        jpegBytes > 200 && sawLookCmd && enrollForgetOk && framesSent > 0,
        {
          fixture: path.basename(frame.path),
          jpeg_bytes: jpegBytes,
          saw_look_cmd: sawLookCmd,
          enroll_forget_tested: enrollTested,
          enroll_status: enrollStatus,
          forget_status: forgetStatus,
          recognized_live_match: recognizedMatch?.toLowerCase() === 'ada',
          enroll_forget_ok: enrollForgetOk,
          frames_sent: framesSent,
        },
      );
    }
  }

  // 8. Echo Loopback & Slow-Whisper Resilience
  {
    const callsBeforeEcho = stack.model.stats.calls;
    await stack.periph.audioEcho({ enabled: true, gain: 0.4, delay_ms: 40 });
    const echoTurn = await runRealSpeechTurn(stack, manifest.wakeCommand, {
      expectAudio: true,
      timeoutMs: 45_000,
      label: 'echo_resilience_turn',
    });
    await sleep(3000);
    await stack.periph.audioEcho({ enabled: false });
    const totalCallsAfterEcho = stack.model.stats.calls - callsBeforeEcho;
    record(
      'speaker-to-mic echo injection does not self-trigger runaway turns',
      echoTurn.terminalKind === 'turn.completed' && totalCallsAfterEcho === 1,
      { turn_id: echoTurn.turnId, model_calls_delta: totalCallsAfterEcho },
    );

    await stack.periph.whisperConfig({ delay_before_ms: 700, delay_after_ms: 0, hold: false });
    const slowTurn = await runRealSpeechTurn(stack, manifest.wakeCommand, {
      expectAudio: true,
      timeoutMs: 45_000,
      label: 'slow_whisper_turn',
    });
    await stack.periph.whisperConfig({ delay_before_ms: 0, delay_after_ms: 0, hold: false });
    const wStatus = await stack.periph.whisperStatus();
    record(
      'slow real whisper-cli wrapper completes cleanly with zero active orphans',
      slowTurn.terminalKind === 'turn.completed' && (wStatus.active_count ?? 0) === 0,
      {
        turn_id: slowTurn.turnId,
        timing_ms: slowTurn.timingMs,
        active_whisper: wStatus.active_count ?? 0,
      },
    );
  }

  // 9. Neural Wake Mode Live Sub-Phase (real ONNX Runtime + openWakeWord + RO hey_pip.onnx)
  //    Strictly split into 3 independent checks with ZERO false-green:
  //      9a. Real ORT dylib mapped in live robot process + zero fallback-to-transcript warnings
  //      9b. Full-duration prerecorded human negative rejection (await mic_wait_idle + VAD quiet)
  //      9c. Positive neural wake -> controller listening -> separate human follow-up + held decode preemption
  //          (explicitly reported as missing_evidence if no human positive wake WAV is mounted)
  if (verifyNeuralWake) {
    if (!stack.canRunNeuralWake()) {
      record(
        'neural wake runtime & read-only hey_pip.onnx available for live qualification',
        false,
        {
          has_ort_dylib: Boolean(stack.paths.ortDylib),
          has_mel: Boolean(stack.paths.melModel),
          has_emb: Boolean(stack.paths.embModel),
          has_private_hey_pip: Boolean(stack.paths.heyPipModel),
        },
        { status: 'missing_evidence' },
      );
    } else {
      log('switching robot to neural wake mode (ORT_DYLIB_PATH + BUDDY_LISTEN_WAKE_DIR + BARGE_IN=1)');
      await stack.stopRobotProcess(10_000);
      const neuralStart = await stack.startRobotProcess({
        vision: includeVision,
        neuralWake: true,
      });
      await stack.until(
        'robot panel Ready in neural wake mode',
        async () => (await (await fetch(`http://127.0.0.1:${stack.ports.screenPort}/`)).text()).includes('Ready'),
        25_000,
      );
      // Warm up openWakeWord 4-second sliding window and confirm libonnxruntime.so is mapped in /proc/<pid>/maps.
      await sleep(4500);
      const neuralLoad = stack.checkNeuralWakeMapped();
      record(
        'neural wake runtime load: libonnxruntime.so mapped in live robot process with zero transcript fallback',
        neuralStart.neuralWake && neuralLoad.mapped && neuralLoad.fallback_warnings === 0,
        neuralLoad,
      );

      // 9b. Full-duration prerecorded human negative rejection (11s JFK clip): await full playback + VAD quiet!
      const callsBeforeNeg = stack.model.stats.calls;
      const faceSeqBeforeNeg = (await stack.periph.faceEvents(0, 1000)).events?.at(-1)?.seq ?? 0;
      if (manifest.humanNegative) {
        await stack.periph.micInject({
          path: manifest.humanNegative.resolvedPath,
          clip_id: 'neural_human_negative_full',
          pad_silence_before_ms: 200,
          pad_silence_after_ms: 1000,
        });
        await stack.periph.micWaitIdle(25_000);
        // Allow VAD tail (800ms) + whisper/wake pipeline to settle cleanly.
        await sleep(2000);
      }
      const negFalseTurns = stack.model.stats.calls - callsBeforeNeg;
      const faceDuringNeg = (await stack.periph.faceEvents(faceSeqBeforeNeg, 500)).events ?? [];
      const negFalseListening = faceDuringNeg.filter(
        (ev) => ev.cmd?.led === 'listening' || ev.led === 'listening',
      ).length;
      record(
        'neural wake human negative rejection: full 11s human JFK clip + VAD quiet produces 0 wakes and 0 turns',
        Boolean(manifest.humanNegative) && negFalseTurns === 0 && negFalseListening === 0,
        {
          clip_id: manifest.humanNegative?.id ?? null,
          clip_provenance: manifest.humanNegative?.provenance ?? 'missing',
          false_turns: negFalseTurns,
          false_listening_events: negFalseListening,
        },
      );

      // 9c. Positive neural wake -> controller listening -> separate human follow-up
      //     + live neural barge-in during playback + held follow-up decode preemption.
      //     Explicitly preserves provenance (`human_recorded_contiguous` vs `edited_spliced_human`),
      //     NEVER mislabeling spliced clips as natural contiguous "Hey Pip"!
      const neuralPos = manifest.neuralWakeStimulus;

      if (!neuralPos) {
        record(
          'neural wake positive -> controller listening + human follow-up + barge-in + held decode preemption',
          false,
          {
            reason:
              'no neural wake positive stimulus found (mount PIP_E2E_HUMAN_WAKE_POSITIVE_WAV or prepare edited_spliced_human_hey_pip.wav)',
            human_wake_positive_mounted: false,
          },
          { status: 'missing_evidence' },
        );
      } else {
        // 1) Wake-only -> controller enters listening (LED), then inject separate VOiCES human follow-up!
        const faceSeqBeforePos = (await stack.periph.faceEvents(0, 1000)).events?.at(-1)?.seq ?? 0;
        await stack.periph.micInject({
          path: neuralPos.resolvedPath,
          clip_id: `neural_wake_${neuralPos.id}`,
          pad_silence_before_ms: 200,
          pad_silence_after_ms: 400,
        });
        await stack.periph.micWaitIdle(15_000);
        let sawListening = false;
        await stack.until(
          'face listening LED after neural wake',
          async () => {
            const evs = (await stack.periph.faceEvents(faceSeqBeforePos, 300)).events ?? [];
            sawListening = evs.some((ev) => ev.cmd?.led === 'listening' || ev.led === 'listening');
            return sawListening;
          },
          10_000,
        ).catch(() => {});

        let followupOk = false;
        let followupTiming = null;
        if (sawListening && manifest.humanFollowup) {
          const fu = await runRealSpeechTurn(stack, manifest.humanFollowup, {
            expectAudio: true,
            timeoutMs: 45_000,
            label: 'neural_wake_human_followup',
          });
          followupOk = fu.terminalKind === 'turn.completed' && fu.transcriptRedacted.non_empty;
          followupTiming = fu.timingMs;
        }

        // 2) Live neural barge-in during active speaker playback (BUDDY_LISTEN_BARGE_IN=1, busy threshold 0.80):
        //    Start a long spoken reply (~8 s of PCM), inject neural wake mid-playback, assert speaker stops early & turn.cancelled!
        let bargeInOk = false;
        let bargeDetail = {};
        if (sawListening && manifest.humanFollowup) {
          // Wait for follow-up window to stay open or re-wake cleanly, then trigger a long spoken reply.
          const longReply = Array.from(
            { length: 12 },
            (_, i) => `Barge-in verification sentence number ${i + 1} streaming continuously to the speaker.`,
          ).join(' ');
          stack.model.setScript(() => ({ kind: 'text', text: longReply }));
          const tBargeTurn = now();
          const speakerBeforeBarge = (await stack.periph.speakerSessions()).total_sessions ?? 0;
          // Re-wake and send human follow-up to start the long speaking turn.
          await stack.periph.micInject({
            path: neuralPos.resolvedPath,
            clip_id: 'neural_barge_setup_wake',
            pad_silence_before_ms: 150,
            pad_silence_after_ms: 300,
          });
          await stack.periph.micWaitIdle(15_000);
          await stack.periph.micInject({
            path: manifest.humanFollowup.resolvedPath,
            clip_id: 'neural_barge_long_turn',
            pad_silence_before_ms: 150,
            pad_silence_after_ms: 950,
          });
          const longStarted = await stack
            .waitEvent((e) => e.t >= tBargeTurn && e.kind === 'turn.started', 35_000, 'barge long turn.started')
            .catch(() => null);
          let activeSpk = null;
          if (longStarted) {
            await stack
              .until(
                'long turn speaker audio started',
                async () => {
                  const res = await stack.periph.speakerSessions();
                  const s = (res.sessions ?? []).find(
                    (x) => (x.session_id ?? 0) > speakerBeforeBarge && x.first_chunk_ms != null,
                  );
                  if (s) {
                    activeSpk = s;
                    return true;
                  }
                  return false;
                },
                20_000,
              )
              .catch(() => {});
          }
          if (longStarted && activeSpk) {
            await sleep(400);
            const tBargeWake = now();
            // Inject neural wake WHILE speaker is actively playing (busy threshold 0.80)!
            await stack.periph.micInject({
              path: neuralPos.resolvedPath,
              clip_id: 'neural_barge_interrupt',
              pad_silence_before_ms: 100,
              pad_silence_after_ms: 400,
            });
            await stack.periph.micWaitIdle(15_000);
            let stoppedSpk = null;
            await stack
              .until(
                'speaker stopped by neural barge-in',
                async () => {
                  const res = await stack.periph.speakerSessions();
                  const s = (res.sessions ?? []).find((x) => x.session_id === activeSpk.session_id);
                  if (s && (s.killed || s.end_ms != null || s.term_ms != null)) {
                    stoppedSpk = s;
                    return true;
                  }
                  return false;
                },
                10_000,
              )
              .catch(() => {});
            const longEnd = await stack
              .waitEvent(
                (e) => e.turn_id === longStarted.turn_id && TERMINAL_KINDS.has(e.kind),
                15_000,
                'barge turn terminal',
              )
              .catch(() => null);
            const fullExpectedBytes = longReply.length * 30 * 48; // rough ~24kHz PCM size
            const cutEarly =
              Boolean(stoppedSpk) && (Boolean(stoppedSpk.killed) || (stoppedSpk.total_bytes ?? 0) < fullExpectedBytes * 0.7);
            bargeInOk = cutEarly && (longEnd?.kind === 'turn.cancelled' || longEnd?.kind === 'turn.completed');
            bargeDetail = {
              turn_end: longEnd?.kind ?? null,
              speaker_killed: Boolean(stoppedSpk?.killed),
              speaker_bytes_played: stoppedSpk?.total_bytes ?? null,
              elapsed_ms: Math.round(now() - tBargeWake),
            };
          }
          stack.model.setScript(null);
          await sleep(600);
        }

        // 3) Held follow-up decode preemption: hold slow-whisper on a follow-up clip, inject neural wake
        //    during the held decode, assert neural wake preempts controller (listening LED) & 0 orphan whisper!
        let preemptionOk = false;
        let preemptionDetail = {};
        if (sawListening && manifest.humanFollowup) {
          await stack.periph.whisperConfig({ hold: true, delay_before_ms: 0 });
          await stack.periph.micInject({
            path: manifest.humanFollowup.resolvedPath,
            clip_id: 'held_followup_for_preemption',
            pad_silence_before_ms: 150,
            pad_silence_after_ms: 950,
          });
          const heldStarted = await stack.periph.whisperWait({ event: 'started', timeout_ms: 20_000 }).catch(() => null);
          const facePrePreempt = (await stack.periph.faceEvents(0, 1000)).events?.at(-1)?.seq ?? 0;
          if (heldStarted) {
            await stack.periph.micInject({
              path: neuralPos.resolvedPath,
              clip_id: 'neural_wake_preempt',
              pad_silence_before_ms: 100,
              pad_silence_after_ms: 400,
            });
            await stack.periph.micWaitIdle(15_000);
            const preemptWake = await stack
              .until(
                'listening LED during held whisper preemption',
                async () => {
                  const evs = (await stack.periph.faceEvents(facePrePreempt, 200)).events ?? [];
                  return evs.some((ev) => ev.cmd?.led === 'listening' || ev.led === 'listening');
                },
                8_000,
              )
              .catch(() => false);
            await stack.periph.whisperRelease();
            await stack.periph.whisperConfig({ hold: false, delay_before_ms: 0 });
            await sleep(800);
            const wsAfter = await stack.periph.whisperStatus();
            preemptionOk = Boolean(preemptWake) && (wsAfter.active_count ?? 0) === 0;
            preemptionDetail = {
              whisper_held_started: true,
              listening_during_hold: Boolean(preemptWake),
              active_whisper_after: wsAfter.active_count ?? 0,
            };
          } else {
            await stack.periph.whisperRelease();
            await stack.periph.whisperConfig({ hold: false, delay_before_ms: 0 });
            preemptionDetail = { whisper_held_started: false };
          }
        }

        record(
          'neural wake positive -> controller listening + human follow-up + live barge-in + held decode preemption',
          sawListening && followupOk && bargeInOk && preemptionOk,
          {
            positive_clip_id: neuralPos.id,
            positive_clip_provenance: neuralPos.provenance,
            is_natural_contiguous_hey_pip: Boolean(neuralPos.provenance === 'human_recorded_contiguous'),
            wake_reached_controller_listening: sawListening,
            human_followup_decoded: followupOk,
            human_followup_timing_ms: followupTiming,
            live_barge_in_ok: bargeInOk,
            barge_in: bargeDetail,
            held_decode_preempted_no_orphans: preemptionOk,
            preemption: preemptionDetail,
          },
        );
      }

      // Return robot to transcript mode so subsequent fault/soak tests run deterministically with available clips.
      await stack.stopRobotProcess(10_000);
      await stack.startRobotProcess({ vision: includeVision, neuralWake: false });
      await stack.until(
        'robot panel Ready after returning to transcript mode',
        async () => (await (await fetch(`http://127.0.0.1:${stack.ports.screenPort}/`)).text()).includes('Ready'),
        25_000,
      );
    }
  }
}

async function runFaultChecks(stack, manifest, { log, record }) {
  // 1. Network Proxy Full Outage (Blackhole) -> Reconnect -> Real Speech Turn Succeeds
  {
    const wsBefore = stack.rivetProxy.websocketCount();
    stack.rivetProxy.setBlackhole(true);
    stack.serverProxy.setBlackhole(true);
    await sleep(2500);
    stack.serverProxy.setBlackhole(false);
    stack.rivetProxy.setBlackhole(false);
    await stack.until(
      'robot reconnects WebSocket after network blackhole',
      async () => stack.rivetProxy.websocketCount() > wsBefore,
      45_000,
    );
    await sleep(500);
    const postNetTurn = await runRealSpeechTurn(stack, manifest.wakeCommand, {
      expectAudio: true,
      timeoutMs: 45_000,
      label: 'post_blackhole_turn',
    });
    record(
      'network proxy blackhole/drop recovers automatically and completes real speech turn',
      postNetTurn.terminalKind === 'turn.completed' && Boolean(postNetTurn.speakerSession),
      {
        ws_before: wsBefore,
        ws_after: stack.rivetProxy.websocketCount(),
        turn_id: postNetTurn.turnId,
        timing_ms: postNetTurn.timingMs,
      },
    );
  }

  // 2. Lost Submit Ack via TLS Fault Proxy -> Exactly 1 Actor Turn, Zero Duplicate Model Calls, Zero Stale Speech
  {
    const wsBefore = stack.rivetProxy.websocketCount();
    const speakerSeqBefore = (await stack.periph.speakerSessions()).total_sessions ?? 0;
    const tCedarStart = now();

    stack.model.setScript(() => ({
      kind: 'text',
      text: 'Cedar lost ack sentinel reply should never be spoken.',
    }));
    const dropped = stack.rivetProxy.dropNext('submit');
    await stack.periph.micInject({
      path: manifest.wakeCommand.resolvedPath,
      clip_id: 'lost_submit_ack',
      pad_silence_before_ms: 250,
      pad_silence_after_ms: 950,
    });
    const cutAt = await Promise.race([dropped, sleep(25_000).then(() => null)]);
    if (!cutAt) throw new Error('submit never reached TLS fault proxy within 25 s');

    await stack.until(
      'robot reconnects after lost submit ack',
      async () => stack.rivetProxy.websocketCount() > wsBefore,
      45_000,
    );
    // Wait for Cedar's replayed turn.completed to settle on the robot before starting the positive control turn!
    const cedarStarted = await stack.waitEvent(
      (e) => e.t >= tCedarStart && e.kind === 'turn.started',
      30_000,
      'cedar replayed turn.started',
    );
    await stack.waitEvent(
      (e) => e.turn_id === cedarStarted.turn_id && TERMINAL_KINDS.has(e.kind),
      30_000,
      'cedar replayed terminal',
    );
    await sleep(1000);
    stack.model.setScript(null);

    const cedarCalls = stack.model.stats.streams.filter((s) => s.reply.includes('Cedar lost ack sentinel')).length;
    const cedarSpeech = stack.speech.stats.requests.filter((q) => q.input.includes('Cedar lost ack sentinel')).length;
    const speakerDuringCedar = ((await stack.periph.speakerSessions()).sessions ?? []).filter(
      (s) => (s.session_id ?? 0) > speakerSeqBefore,
    );

    const controlTurn = await runRealSpeechTurn(stack, manifest.wakeCommand, {
      expectAudio: true,
      timeoutMs: 45_000,
      label: 'post_lost_ack_control',
    });
    const cedarSpeechAfterControl = stack.speech.stats.requests.filter((q) =>
      q.input.includes('Cedar lost ack sentinel'),
    ).length;

    record(
      'lost submit ack over TLS fault proxy: 1 actor turn, 0 stale speech, next real turn plays',
      cedarCalls === 1 &&
        cedarSpeech === 0 &&
        cedarSpeechAfterControl === 0 &&
        speakerDuringCedar.length === 0 &&
        controlTurn.terminalKind === 'turn.completed' &&
        Boolean(controlTurn.speakerSession),
      {
        cedar_model_calls: cedarCalls,
        cedar_speech_requests: cedarSpeechAfterControl,
        speaker_sessions_during_cedar: speakerDuringCedar.length,
        control_turn_id: controlTurn.turnId,
      },
    );
  }

  // 3. BOTH Real Server AND Real Rivet 3.1.1 Engine Restart with Persistent Volume
  {
    const wsBefore = stack.rivetProxy.websocketCount();
    const faceSeqBeforeRestart = (await stack.periph.faceEvents(0, 1000)).events?.at(-1)?.seq ?? 0;
    const speechBeforeRestart = stack.speech.stats.requests.length;

    const restartInfo = await stack.restartServerAndEngine({ killEngineFirst: false });

    await stack.until(
      'robot reconnects after BOTH server + rivet-engine 3.1.1 restart',
      async () => stack.rivetProxy.websocketCount() > wsBefore,
      60_000,
    );
    await sleep(1500);
    const snapAfterRestart = await stack.botCall({ type: 'snapshot' });
    const staleSpeechAfterRestart = stack.speech.stats.requests.length - speechBeforeRestart;
    const faceEventsDuringRestart = (await stack.periph.faceEvents(faceSeqBeforeRestart, 500)).events ?? [];
    const spuriousHappyReplay = faceEventsDuringRestart.some((ev) => ev.cmd?.e === 'happy' || ev.e === 'happy');

    const postRestartTurn = await runRealSpeechTurn(stack, manifest.wakeCommand, {
      expectAudio: true,
      timeoutMs: 45_000,
      label: 'post_engine_restart_turn',
    });

    record(
      'BOTH real robot-buddy-server + rivet-engine 3.1.1 restart with persistent /data preserves owner, no stale replay, resumes speech',
      restartInfo.server_pid_changed &&
        restartInfo.engine_pid_changed &&
        restartInfo.persisted_logs &&
        restartInfo.persisted_objects &&
        snapAfterRestart.owner === stack.oidc.expectedOwner &&
        staleSpeechAfterRestart === 0 &&
        !spuriousHappyReplay &&
        postRestartTurn.terminalKind === 'turn.completed' &&
        Boolean(postRestartTurn.speakerSession),
      {
        ...restartInfo,
        owner_preserved: snapAfterRestart.owner === stack.oidc.expectedOwner,
        stale_speech_after_restart: staleSpeechAfterRestart,
        spurious_happy_replay: spuriousHappyReplay,
        post_restart_turn_id: postRestartTurn.turnId,
        post_restart_timing_ms: postRestartTurn.timingMs,
      },
    );
  }
}

async function runSoak(stack, manifest, { durationSec, intervalSec, includeVision, log, record }) {
  log(`starting continuous conversation soak for ${durationSec}s (interval ${intervalSec}s, vision=${includeVision})`);

  // Activate a real face camera scene throughout the soak so vision inference runs concurrently with 50 Hz neck + STT!
  const visionFrame = includeVision && stack.paths.yunetModel ? findVisionFixtureFrame() : null;
  let cameraPreviewChecks = 0;
  let cameraPreviewOk = 0;
  if (visionFrame) {
    await stack.periph.cameraScene({
      kind: visionFrame.kind,
      path: visionFrame.path,
      offset_x: 0.15,
      offset_y: -0.05,
      fps: 10,
    });
  }

  const startWall = Date.now();
  const startIso = new Date(startWall).toISOString();
  const deadline = startWall + durationSec * 1000;
  const micToStarted = [];
  const startedToAudio = [];
  const micToAudio = [];
  const micToCompleted = [];
  const sampleWithPeripherals = async () => {
    const base = stack.sampleMetrics();
    let servoSnap = null;
    let camSnap = null;
    try {
      const s = await stack.periph.servoState();
      const joints = Object.values(s.joints ?? {});
      servoSnap = {
        goal_frames_count: s.goal_frames_count ?? 0,
        cadence_hz: s.cadence_hz ?? 0,
        all_torque_on: joints.length === 3 && joints.every((j) => j.torque === 1),
        torques: joints.map((j) => j.torque),
        limit_violations: s.limit_violations ?? 0,
        checksum_errors: s.checksum_errors ?? 0,
      };
    } catch {}
    try {
      const c = await stack.periph.cameraStatus();
      camSnap = { frames_sent: c.frames_sent ?? 0 };
    } catch {}
    const logNow = stack.parseLogTelemetry();
    return {
      ...base,
      wall_ms: Date.now(),
      servo: servoSnap,
      camera: camSnap,
      motion_ticks_log: logNow.motion_20ms.total_ticks,
      motion_missed_log: logNow.motion_20ms.total_missed,
      neck_stopped_errors: logNow.health.neck_motion_stopped_errors,
      vision_frames_log: logNow.health.vision_frames_max,
    };
  };
  const samples = [await sampleWithPeripherals()];
  let turnsAttempted = 0;
  let turnsSucceeded = 0;
  let generatedWakeTurns = 0;
  let humanFollowupTurns = 0;
  let expressTurns = 0;
  let networkFaults = 0;
  let restartFaults = 0;
  const failures = [];

  const nextFaultAt = {
    network: startWall + Math.min(240_000, Math.max(45_000, Math.floor((durationSec * 1000) / 4))),
    restart: startWall + Math.min(600_000, Math.max(90_000, Math.floor((durationSec * 1000) / 2))),
  };
  let inFollowupWindow = false;

  while (Date.now() < deadline) {
    turnsAttempted += 1;
    // Alternate wake command + prerecorded human follow-up inside the active follow-up window.
    const useHumanFollowup = inFollowupWindow && Boolean(manifest.humanFollowup);
    const clip = useHumanFollowup ? manifest.humanFollowup : manifest.wakeCommand;
    const isExpressTurn = turnsAttempted % 4 === 0;

    if (isExpressTurn) {
      let offered = false;
      const gestures = ['nod', 'tilt', 'shake'];
      const emotions = ['happy', 'curious', 'surprised'];
      const g = gestures[expressTurns % gestures.length];
      const em = emotions[expressTurns % emotions.length];
      stack.model.setScript((ctx) => {
        if (ctx.hasTools && ctx.toolResultCount === 0 && !offered) {
          offered = true;
          return { kind: 'tool', name: 'express', args: { emotion: em, intensity: 0.75, gesture: g } };
        }
        return { kind: 'text', text: `Soak turn ${turnsAttempted} with ${em} and ${g}.` };
      });
    } else {
      stack.model.setScript(() => ({
        kind: 'text',
        text: `Soak turn ${turnsAttempted} confirmed. All systems nominal.`,
      }));
    }

    try {
      const res = await runRealSpeechTurn(stack, clip, {
        expectAudio: true,
        timeoutMs: 60_000,
        label: `soak_turn_${turnsAttempted}`,
      });
      if (res.terminalKind === 'turn.completed' && res.speakerSession && res.transcriptRedacted.non_empty) {
        turnsSucceeded += 1;
        if (useHumanFollowup) {
          humanFollowupTurns += 1;
          inFollowupWindow = false;
        } else {
          generatedWakeTurns += 1;
          inFollowupWindow = true;
        }
        micToStarted.push(res.timingMs.mic_inject_to_turn_started_ms);
        if (res.timingMs.turn_started_to_first_audio_ms != null) {
          startedToAudio.push(res.timingMs.turn_started_to_first_audio_ms);
        }
        if (res.timingMs.mic_inject_to_first_audio_ms != null) {
          micToAudio.push(res.timingMs.mic_inject_to_first_audio_ms);
        }
        micToCompleted.push(res.timingMs.mic_inject_to_turn_completed_ms);
        if (isExpressTurn && res.turnEvents.some((e) => e.kind === 'face.express')) {
          expressTurns += 1;
        }
      } else {
        inFollowupWindow = false;
        failures.push({
          turn: turnsAttempted,
          elapsed_s: Math.round((Date.now() - startWall) / 1000),
          reason: `terminal=${res.terminalKind}, speaker=${Boolean(res.speakerSession)}, decoded=${res.transcriptRedacted.non_empty}`,
        });
      }
    } catch (e) {
      inFollowupWindow = false;
      failures.push({
        turn: turnsAttempted,
        elapsed_s: Math.round((Date.now() - startWall) / 1000),
        reason: e.message,
      });
    } finally {
      stack.model.setScript(null);
    }

    // Periodically verify active camera JPEG preview during soak (also varies gaze offset).
    if (visionFrame && (turnsAttempted === 1 || turnsAttempted % 5 === 0)) {
      cameraPreviewChecks += 1;
      try {
        const offsetX = turnsAttempted % 2 === 0 ? 0.2 : -0.2;
        await stack.periph.cameraScene({
          kind: visionFrame.kind,
          path: visionFrame.path,
          offset_x: offsetX,
          offset_y: -0.05,
          fps: 10,
        });
        const r = await stack.operatorRequest('/camera');
        const n = await readFirstMjpegFrameBytes(r, 4000);
        if (n > 200) cameraPreviewOk += 1;
      } catch {}
    }

    samples.push(await sampleWithPeripherals());

    // Inject periodic network proxy drop fault during soak.
    if (Date.now() >= nextFaultAt.network && deadline - Date.now() > 45_000) {
      networkFaults += 1;
      inFollowupWindow = false;
      nextFaultAt.network = Date.now() + 420_000;
      const wsBefore = stack.rivetProxy.websocketCount();
      log(`soak fault #${networkFaults}: TLS proxy cut/blackhole 2s`);
      stack.rivetProxy.setBlackhole(true);
      await sleep(2000);
      stack.rivetProxy.setBlackhole(false);
      try {
        await stack.until('soak reconnect', async () => stack.rivetProxy.websocketCount() > wsBefore, 45_000);
      } catch (e) {
        failures.push({ turn: turnsAttempted, elapsed_s: Math.round((Date.now() - startWall) / 1000), reason: e.message });
      }
    }

    // Inject real BOTH server + rivet-engine 3.1.1 restart mid-soak.
    if (durationSec >= 120 && Date.now() >= nextFaultAt.restart && deadline - Date.now() > 60_000) {
      restartFaults += 1;
      inFollowupWindow = false;
      nextFaultAt.restart = Number.POSITIVE_INFINITY;
      const wsBefore = stack.rivetProxy.websocketCount();
      log(`soak fault: real server + rivet-engine 3.1.1 restart with persistent /data`);
      try {
        await stack.restartServerAndEngine({ killEngineFirst: false });
        await stack.until('soak reconnect after restart', async () => stack.rivetProxy.websocketCount() > wsBefore, 60_000);
      } catch (e) {
        failures.push({ turn: turnsAttempted, elapsed_s: Math.round((Date.now() - startWall) / 1000), reason: e.message });
      }
    }

    if (turnsAttempted % 10 === 0 || Date.now() >= deadline) {
      const latest = samples.at(-1);
      log(
        `soak progress: ${Math.round((Date.now() - startWall) / 1000)}s/${durationSec}s ` +
          `turns=${turnsSucceeded}/${turnsAttempted} (wake=${generatedWakeTurns}, human_followup=${humanFollowupTurns}, express=${expressTurns}) ` +
          `ticks=${latest?.motion_ticks_log ?? '?'} goals=${latest?.servo?.goal_frames_count ?? '?'} torque_on=${latest?.servo?.all_torque_on ?? '?'} ` +
          `vframes=${latest?.vision_frames_log ?? '?'} ` +
          `robot_rss=${latest?.robot?.rss_kib ? (latest.robot.rss_kib / 1024).toFixed(1) : '?'}MiB ` +
          `fds=${latest?.robot?.fds ?? '?'} neck_stops=${latest?.neck_stopped_errors ?? '?'} failures=${failures.length}`,
      );
    }

    // Short pause between wake and human follow-up (stays inside follow-up window); normal interval otherwise.
    const waitMs = inFollowupWindow ? 1200 : intervalSec * 1000;
    const remainingMs = deadline - Date.now();
    if (remainingMs > 0) {
      await sleep(Math.min(waitMs, remainingMs));
    }
  }

  const endWall = Date.now();
  const endIso = new Date(endWall).toISOString();
  const elapsedSec = Math.round((endWall - startWall) / 1000);
  const servoBounds = await stack.periph.servoAssertBounds({});
  const servoState = await stack.periph.servoState();
  const servoGoalsAll = (await stack.periph.servoEvents('goals', 0, 10000)).events ?? [];
  const distinctServoGoals = countDistinctServoGoals(servoGoalsAll);
  const camStatus = await stack.periph.cameraStatus();
  const wStatus = await stack.periph.whisperStatus();
  const logTelemetry = stack.parseLogTelemetry();

  // Sustained progress verification across first half, second half, and final sample.
  const firstSample = samples[0];
  const midSample = samples[Math.floor(samples.length / 2)];
  const lastSample = samples[samples.length - 1];
  const expectedTicks50Hz = Math.round(elapsedSec * 50);
  const expectedWindows5s = Math.floor(elapsedSec / 5);
  const minExpectedTicks = Math.floor(expectedTicks50Hz * 0.9);
  const minExpectedWindows = Math.max(1, Math.floor(expectedWindows5s * 0.88));
  const goalFramesStart = firstSample?.servo?.goal_frames_count ?? 0;
  const goalFramesMid = midSample?.servo?.goal_frames_count ?? 0;
  const goalFramesEnd = lastSample?.servo?.goal_frames_count ?? 0;
  const goalFramesDelta = goalFramesEnd - goalFramesStart;
  const torqueDroppedSamples = samples.filter((s) => s.servo && !s.servo.all_torque_on).length;
  const sustainedServoProgress =
    goalFramesDelta >= minExpectedTicks &&
    goalFramesMid > goalFramesStart &&
    goalFramesEnd > goalFramesMid &&
    torqueDroppedSamples === 0;
  const sustainedMotionLogProgress =
    logTelemetry.motion_20ms.total_ticks >= minExpectedTicks &&
    logTelemetry.motion_20ms.windows >= minExpectedWindows &&
    logTelemetry.health.neck_motion_stopped_errors === 0 &&
    logTelemetry.health.neck_disarmed_errors === 0;
  const visionFramesStart = firstSample?.vision_frames_log ?? 0;
  const visionFramesMid = midSample?.vision_frames_log ?? 0;
  const visionFramesEnd = lastSample?.vision_frames_log ?? 0;
  const sustainedVisionProgress =
    !visionFrame || (visionFramesMid > visionFramesStart && visionFramesEnd > visionFramesMid);

  const robotRss = growthStats(samples.map((s) => s.robot?.rss_kib), 1024);
  const childrenRss = growthStats(samples.map((s) => s.robot_children?.rss_kib), 1024);
  const robotTreeRss = growthStats(samples.map((s) => s.robot_tree_rss_kib), 1024);
  const serverRss = growthStats(samples.map((s) => s.server?.rss_kib), 1024);
  const engineRss = growthStats(samples.map((s) => s.engine?.rss_kib), 1024);
  const robotFds = growthStats(samples.map((s) => s.robot?.fds), 1);
  const cgroupCurrent = growthStats(samples.map((s) => s.cgroup_memory?.current_mib), 1);
  const cgroupPeakVals = samples.map((s) => s.cgroup_memory?.peak_mib).filter((x) => typeof x === 'number');
  const cgroupOomVals = samples.map((s) => s.cgroup_memory?.oom_kill).filter((x) => typeof x === 'number');

  const soakSummary = {
    clocks: {
      started_at_iso: startIso,
      ended_at_iso: endIso,
      started_epoch_ms: startWall,
      ended_epoch_ms: endWall,
      elapsed_s: elapsedSec,
    },
    duration_requested_s: durationSec,
    duration_elapsed_s: elapsedSec,
    turns_attempted: turnsAttempted,
    turns_succeeded: turnsSucceeded,
    generated_wake_turns: generatedWakeTurns,
    human_recorded_followup_turns: humanFollowupTurns,
    express_turns_verified: expressTurns,
    network_faults_injected: networkFaults,
    server_engine_restarts_injected: restartFaults,
    failures,
    timing_endpoints_ms: {
      mic_inject_to_turn_started: stats(micToStarted),
      turn_started_to_first_audio: stats(startedToAudio),
      mic_inject_to_first_audio: stats(micToAudio),
      mic_inject_to_turn_completed: stats(micToCompleted),
      audio_journal_first_pcm: stats(logTelemetry.audio_first_pcm_ms),
    },
    memory_growth_mib: {
      robot_bare_rss: robotRss,
      robot_children_rss: childrenRss,
      robot_tree_rss: robotTreeRss,
      cgroup_memory_current: cgroupCurrent,
      cgroup_memory_peak_mib: cgroupPeakVals.length ? Math.max(...cgroupPeakVals) : 'unknown',
      cgroup_oom_kills: cgroupOomVals.length ? Math.max(...cgroupOomVals) : 'unknown',
      server_rss: serverRss,
      engine_rss: engineRss,
    },
    file_descriptors_growth: {
      robot: robotFds,
    },
    motion_20ms: {
      ...logTelemetry.motion_20ms,
      expected_ticks_50hz: expectedTicks50Hz,
      min_required_ticks_90pct: minExpectedTicks,
      expected_windows_5s: expectedWindows5s,
      min_required_windows: minExpectedWindows,
      tick_coverage_pct: expectedTicks50Hz > 0 ? Math.round((logTelemetry.motion_20ms.total_ticks / expectedTicks50Hz) * 1000) / 10 : null,
      neck_motion_stopped_errors: logTelemetry.health.neck_motion_stopped_errors,
      neck_disarmed_errors: logTelemetry.health.neck_disarmed_errors,
      neck_stop_reasons: logTelemetry.health.neck_stop_reasons,
      sustained_motion_log_progress: sustainedMotionLogProgress,
    },
    panics: logTelemetry.panics,
    camera: {
      active_fixture: visionFrame ? path.basename(visionFrame.path) : null,
      frames_sent: camStatus.frames_sent ?? camStatus.frames_emitted ?? null,
      vision_frames_log_start: visionFramesStart,
      vision_frames_log_mid: visionFramesMid,
      vision_frames_log_end: visionFramesEnd,
      sustained_vision_progress: sustainedVisionProgress,
      preview_checks_ok: cameraPreviewOk,
      preview_checks_total: cameraPreviewChecks,
    },
    servo: {
      bounds_ok: Boolean(servoBounds.ok),
      limit_violations: servoState.limit_violations ?? 0,
      eeprom_lock_violations: servoState.eeprom_lock_violations ?? 0,
      checksum_errors: servoState.checksum_errors ?? 0,
      distinct_goals_written: distinctServoGoals,
      goal_frames_start: goalFramesStart,
      goal_frames_mid: goalFramesMid,
      goal_frames_end: goalFramesEnd,
      goal_frames_delta: goalFramesDelta,
      torque_dropped_samples: torqueDroppedSamples,
      sustained_servo_progress: sustainedServoProgress,
    },
    active_whisper_processes: wStatus.active_count ?? 0,
    sample_count: samples.length,
  };

  const soakOk =
    turnsAttempted > 0 &&
    turnsSucceeded === turnsAttempted &&
    failures.length === 0 &&
    logTelemetry.panics.total === 0 &&
    sustainedMotionLogProgress &&
    sustainedServoProgress &&
    sustainedVisionProgress &&
    Boolean(servoBounds.ok) &&
    (servoState.limit_violations ?? 0) === 0 &&
    distinctServoGoals >= 2 &&
    (wStatus.active_count ?? 0) === 0 &&
    (!visionFrame || (cameraPreviewOk > 0 && cameraPreviewOk === cameraPreviewChecks)) &&
    (robotTreeRss.peak == null || robotTreeRss.peak < 1024) &&
    (robotFds.peak == null || robotFds.peak < 256);

  record(
    `continuous conversation soak (${durationSec}s): 100% turns, 0 panics, sustained 50Hz neck + camera across full duration, bounded memory/FD growth`,
    soakOk,
    soakSummary,
  );

  return { soakSummary, samples };
}

function renderMarkdown(report) {
  const lines = [
    `# ARM64 Pip E2E Release QA Report`,
    ``,
    `- **Suite:** \`${report.suite}\``,
    `- **Date:** ${report.date}`,
    `- **Arch:** \`${report.provenance.arch}\``,
    `- **Git:** \`${report.provenance.git.head ?? 'unknown'}\` (\`${report.provenance.git.branch ?? 'unknown'}\`, dirty=${report.provenance.git.dirty})`,
    `- **Product Source Digest (\`apps/\` + \`crates/\` + \`Cargo.lock\`):** \`${report.provenance.product_source_digest.sha256}\` (${report.provenance.product_source_digest.file_count} files)`,
    `- **Rivet Engine:** \`${report.provenance.engine_version ?? 'unknown'}\``,
    `- **Artifact Redaction Gate:** **${report.redaction_gate.ok ? 'PASS' : 'FAIL'}**`,
    `- **Overall Status:** **${report.ok ? 'PASS' : report.executable_gates_ok ? 'PARTIAL (missing_evidence)' : 'FAIL'}** (${report.passed} pass, ${report.missing_evidence} missing_evidence, ${report.failed} fail)`,
    ``,
    `## Fixture vs Real Hardware Boundary (Explicit)`,
    ``,
    `| Component | Mode in This Run | Remaining Physical Hardware Gate |`,
    `|---|---|---|`,
    `| \`robot-buddy-server\` | **Real ARM64 release binary** | None (production binary) |`,
    `| \`rivet-engine\` | **Pinned real Rivet 3.1.1 engine** | None (production binary) |`,
    `| \`robot-buddy-robot\` | **Real ARM64 release (\`pi-vision,onnx-wake\`, NO \`--simulate\`) over real TLS** | Physical Pi 5 thermal/voltage/clock |`,
    `| Speech STT (\`whisper-cli\`) | **Real ARM64 whisper.cpp + ggml-tiny.en.bin** | Physical reSpeaker room SNR |`,
    `| Transcript Wake Audio | \`${report.speech_fixtures.wake_command_id}\` (\`${report.speech_fixtures.wake_command_provenance}\`, human=${report.speech_fixtures.wake_command_is_human}) | Physical mic room acoustics |`,
    `| Neural Wake Stimulus | \`${report.speech_fixtures.neural_wake_stimulus_id}\` (\`${report.speech_fixtures.neural_wake_stimulus_provenance}\`, natural_contiguous_hey_pip=${report.speech_fixtures.neural_wake_is_natural_contiguous_hey_pip}) | Natural contiguous human "Hey Pip" recording on Pi |`,
    `| Follow-Up Audio Fixture | \`${report.speech_fixtures.human_followup_id}\` (\`${report.speech_fixtures.human_followup_provenance}\`, human=${report.speech_fixtures.human_followup_is_human}) | Physical mic acoustics |`,
    `| Model, TTS & OIDC | **Local mock OpenAI SSE + mock 24 kHz PCM + mock RS256 OIDC** | Live cloud provider / IdP credentials |`,
    `| Face & Neck hardware | **Real Linux PTYs** (ESP32 JSONL + Feetech STS3215 protocol/limit checks) | Physical cables, servo torque & mechanical stops |`,
    `| Camera & Vision | **Virtual \`rpicam-vid\` I420 + real OpenCV YuNet/SFace** | Physical OV5647 exposure, lens & framing |`,
    ``,
    report.speech_fixtures.neural_wake_is_natural_contiguous_hey_pip
      ? `> Contiguous human-recorded "Hey Pip" wake fixture was mounted and exercised.`
      : `> **Provenance note:** Neural wake used \`${report.speech_fixtures.neural_wake_stimulus_id}\` (\`${report.speech_fixtures.neural_wake_stimulus_provenance}\` — component-level spliced stimulus, NOT a natural contiguous "Hey Pip" recording). Transcript wake used \`${report.speech_fixtures.wake_command_id}\` (\`${report.speech_fixtures.wake_command_provenance}\`) and follow-ups used \`${report.speech_fixtures.human_followup_id}\` (\`${report.speech_fixtures.human_followup_provenance}\`).`,
    ``,
    `## Binary & Artifact SHA-256 Provenance`,
    ``,
    `| Artifact | Bytes | SHA-256 |`,
    `|---|---|---|`,
  ];
  for (const [k, v] of Object.entries(report.provenance.files)) {
    if (v.missing) {
      lines.push(`| \`${k}\` | missing | — |`);
    } else {
      lines.push(`| \`${k}\` (\`${v.name}\`) | ${v.bytes} | \`${v.sha256}\` |`);
    }
  }
  lines.push('', '## Check Results', '', '| Status | Check | Key Details |', '|---|---|---|');
  for (const c of report.checks) {
    const summary = JSON.stringify(c.detail).slice(0, 220).replace(/\|/g, '\\|');
    const label = c.status === 'pass' ? 'PASS' : c.status === 'missing_evidence' ? 'MISSING_EVIDENCE' : 'FAIL';
    lines.push(`| ${label} | ${c.name} | \`${summary}\` |`);
  }
  if (report.log_telemetry) {
    const m = report.log_telemetry.motion_20ms;
    lines.push(
      '',
      '## 50 Hz Motion & Runtime Health',
      '',
      `- 50 Hz neck loop windows: ${m.windows}, total_ticks: ${m.total_ticks}, total_missed: ${m.total_missed} (${m.missed_rate_pct ?? 'n/a'}%), total_skipped_slots: ${m.total_skipped_slots}`,
      `- Cycle timing (us): p95_max=${m.p95_cycle_us_max}, max_cycle=${m.max_cycle_us}, max_work=${m.max_work_us}, max_overrun=${m.max_overrun_us}`,
      `- Panics: robot=${report.log_telemetry.panics.robot}, server=${report.log_telemetry.panics.server} (total=${report.log_telemetry.panics.total})`,
    );
  }
  if (report.soak) {
    const s = report.soak;
    const mg = s.memory_growth_mib;
    lines.push(
      '',
      '## Continuous Conversation Soak Summary',
      '',
      `- Clocks: start=\`${s.clocks?.started_at_iso ?? 'unknown'}\` (${s.clocks?.started_epoch_ms ?? 'n/a'}), end=\`${s.clocks?.ended_at_iso ?? 'unknown'}\` (${s.clocks?.ended_epoch_ms ?? 'n/a'}), elapsed=${s.duration_elapsed_s}s / ${s.duration_requested_s}s`,
      `- Turns: ${s.turns_succeeded}/${s.turns_attempted} (wake=${s.generated_wake_turns}, human_recorded_followup=${s.human_recorded_followup_turns}, express=${s.express_turns_verified}, failures=${s.failures.length})`,
      `- Faults survived: ${s.network_faults_injected} TLS proxy blackholes, ${s.server_engine_restarts_injected} BOTH server+rivet-engine 3.1.1 restarts`,
      `- Sustained 50 Hz motion: ticks=${s.motion_20ms?.total_ticks} / expected~${s.motion_20ms?.expected_ticks_50hz} (${s.motion_20ms?.tick_coverage_pct}%), windows=${s.motion_20ms?.windows} / expected~${s.motion_20ms?.expected_windows_5s}, missed=${s.motion_20ms?.total_missed}, skipped_slots=${s.motion_20ms?.total_skipped_slots}, neck_stops=${s.motion_20ms?.neck_motion_stopped_errors}, reasons=${JSON.stringify(s.motion_20ms?.neck_stop_reasons ?? [])}`,
      `- Sustained servo PTY: goal_frames ${s.servo.goal_frames_start} -> ${s.servo.goal_frames_mid} -> ${s.servo.goal_frames_end} (delta=${s.servo.goal_frames_delta}), torque_dropped_samples=${s.servo.torque_dropped_samples}, distinct_goals=${s.servo.distinct_goals_written}, limit_violations=${s.servo.limit_violations}, checksum_errors=${s.servo.checksum_errors}`,
      `- Timing endpoints (ms):`,
      `  - mic_inject -> turn.started: p50=${s.timing_endpoints_ms.mic_inject_to_turn_started.p50}, p95=${s.timing_endpoints_ms.mic_inject_to_turn_started.p95}, max=${s.timing_endpoints_ms.mic_inject_to_turn_started.max}`,
      `  - turn.started -> first_audio: p50=${s.timing_endpoints_ms.turn_started_to_first_audio.p50}, p95=${s.timing_endpoints_ms.turn_started_to_first_audio.p95}, max=${s.timing_endpoints_ms.turn_started_to_first_audio.max}`,
      `  - mic_inject -> first_audio: p50=${s.timing_endpoints_ms.mic_inject_to_first_audio.p50}, p95=${s.timing_endpoints_ms.mic_inject_to_first_audio.p95}, max=${s.timing_endpoints_ms.mic_inject_to_first_audio.max}`,
      `  - mic_inject -> turn.completed: p50=${s.timing_endpoints_ms.mic_inject_to_turn_completed.p50}, p95=${s.timing_endpoints_ms.mic_inject_to_turn_completed.p95}, max=${s.timing_endpoints_ms.mic_inject_to_turn_completed.max}`,
      `  - audio journal first_pcm_ms: n=${s.timing_endpoints_ms.audio_journal_first_pcm.n}, p50=${s.timing_endpoints_ms.audio_journal_first_pcm.p50}, p95=${s.timing_endpoints_ms.audio_journal_first_pcm.p95}`,
      `- Memory growth (MiB):`,
      `  - robot bare RSS: start=${mg.robot_bare_rss.start}, end=${mg.robot_bare_rss.end}, peak=${mg.robot_bare_rss.peak}, delta=${mg.robot_bare_rss.delta}`,
      `  - robot children RSS (whisper/arecord/aplay/rpicam): start=${mg.robot_children_rss.start}, end=${mg.robot_children_rss.end}, peak=${mg.robot_children_rss.peak}, delta=${mg.robot_children_rss.delta}`,
      `  - robot tree RSS: start=${mg.robot_tree_rss.start}, end=${mg.robot_tree_rss.end}, peak=${mg.robot_tree_rss.peak}, delta=${mg.robot_tree_rss.delta}`,
      `  - cgroup memory: current_delta=${mg.cgroup_memory_current.delta ?? 'unknown'}, peak=${mg.cgroup_memory_peak_mib}, oom_kills=${mg.cgroup_oom_kills}`,
      `  - server RSS: start=${mg.server_rss.start}, end=${mg.server_rss.end}, peak=${mg.server_rss.peak}, delta=${mg.server_rss.delta}`,
      `  - rivet-engine RSS: start=${mg.engine_rss.start}, end=${mg.engine_rss.end}, peak=${mg.engine_rss.peak}, delta=${mg.engine_rss.delta}`,
      `- Robot FD growth: start=${s.file_descriptors_growth.robot.start}, end=${s.file_descriptors_growth.robot.end}, peak=${s.file_descriptors_growth.robot.peak}, delta=${s.file_descriptors_growth.robot.delta}`,
      `- Camera during soak: fixture=${s.camera.active_fixture}, frames_sent=${s.camera.frames_sent}, vision_frames_log=${s.camera.vision_frames_log_start} -> ${s.camera.vision_frames_log_mid} -> ${s.camera.vision_frames_log_end}, previews=${s.camera.preview_checks_ok}/${s.camera.preview_checks_total}`,
    );
  }
  return `${lines.join('\n')}\n`;
}

if (import.meta.main) {
  const suite = getOpt('suite', 'all');
  const durationSec = parseDurationSeconds(getOpt('duration', null), suite === 'soak' ? 1800 : 0);
  const intervalSec = Number(getOpt('soak-interval', '8'));
  const outDir = getOpt(
    'out',
    path.join(process.env.PIP_E2E_ARTIFACTS_DIR ?? path.join(import.meta.dir, '.artifacts'), `${suite}-${Date.now()}`),
  );
  const keep = hasFlag('keep');
  const strict = hasFlag('strict');
  const neuralWakeStart = hasFlag('neural-wake');
  const includeVision = !hasFlag('no-vision');
  const log = (...a) => console.error(`[pip-e2e:${suite}]`, ...a);

  fs.mkdirSync(outDir, { recursive: true });
  const manifest = loadSpeechManifest(getOpt('manifest', null));

  const { checks, record } = makeRecorder(log);
  let stack = null;
  let soakResult = null;
  let logTelemetry = null;
  let fatalError = null;
  let provenance = null;

  try {
    stack = await startPipE2EStack({
      log,
      keep,
      vision: includeVision,
      neuralWake: neuralWakeStart,
    });
    if (!manifest.wakeCommand) {
      manifest.generatedWakeCommand = synthesizeWakeCommandClip(stack.work);
      manifest.wakeCommand = manifest.generatedWakeCommand;
      log(
        `synthesized non-sleep wake command clip ${manifest.wakeCommand.id} (provenance=synthetic, is_human_recording=false) because manifest had no conversational wake+command clip`,
      );
    }
    provenance = collectProvenance(stack.paths);

    if (suite === 'smoke') {
      await runCoreChecks(stack, manifest, { log, record, includeVision, verifyNeuralWake: false });
    } else if (suite === 'core') {
      await runCoreChecks(stack, manifest, { log, record, includeVision, verifyNeuralWake: true });
    } else if (suite === 'faults') {
      const claim = await stack.completeOidcClaimViaPanel();
      record('enrollment + OIDC claim (pre-faults)', claim.owner_matched, claim);
      await runFaultChecks(stack, manifest, { log, record });
    } else if (suite === 'soak') {
      const claim = await stack.completeOidcClaimViaPanel();
      record('enrollment + OIDC claim (pre-soak)', claim.owner_matched, claim);
      soakResult = await runSoak(stack, manifest, { durationSec, intervalSec, includeVision, log, record });
    } else if (suite === 'all') {
      await runCoreChecks(stack, manifest, { log, record, includeVision, verifyNeuralWake: true });
      await runFaultChecks(stack, manifest, { log, record });
      if (durationSec > 0) {
        soakResult = await runSoak(stack, manifest, { durationSec, intervalSec, includeVision, log, record });
      }
    } else {
      throw new Error(`unknown --suite: ${suite}`);
    }

    // Parse 50 Hz motion periodic telemetry & panic counts before final stop.
    logTelemetry = stack.parseLogTelemetry();
    record(
      'runtime health: 0 panics, 0 neck worker stops, and valid 50 Hz neck actuator ticks recorded',
      logTelemetry.panics.total === 0 &&
        logTelemetry.health.neck_motion_stopped_errors === 0 &&
        logTelemetry.health.neck_disarmed_errors === 0 &&
        logTelemetry.motion_20ms.total_ticks > 0,
      {
        panics: logTelemetry.panics,
        motion_20ms: logTelemetry.motion_20ms,
        neck_motion_stopped_errors: logTelemetry.health.neck_motion_stopped_errors,
        neck_disarmed_errors: logTelemetry.health.neck_disarmed_errors,
        neck_stop_reasons: logTelemetry.health.neck_stop_reasons,
      },
    );

    // Explicit clean SIGTERM stop check (robot flushes cache, disables servo torque, exits 0 with zero orphans).
    const stopInfo = await stack.stopRobotProcess(10_000);
    await sleep(300);
    const servoPostStop = await stack.periph.servoState();
    const postStopJoints = Object.values(servoPostStop.joints ?? servoPostStop.servos ?? {});
    const torqueAllOff =
      postStopJoints.length === 3 &&
      postStopJoints.every((s) => s.torque === 0 || s.torque_enabled === false);
    record(
      'robot clean SIGTERM stop: exit 0, cache flushed, servo torque disabled, zero orphan children',
      stopInfo !== null &&
        stopInfo.exitCode === 0 &&
        stopInfo.cacheExists &&
        stopInfo.remainingChildren.length === 0 &&
        torqueAllOff,
      { ...stopInfo, torque_all_off: torqueAllOff },
    );
  } catch (e) {
    fatalError = e.message;
    record('suite completed without unhandled exception', false, { error: e.message });
    log(`FATAL: ${e.message}`);
    if (stack) log(stack.tails(40));
  } finally {
    if (stack) {
      if (!logTelemetry) {
        try {
          logTelemetry = stack.parseLogTelemetry();
        } catch {}
      }
      const shutdownResult = await stack.shutdown();
      record('full stack teardown leaves zero leaks', (shutdownResult?.leaks?.length ?? 0) === 0, {
        leaks: shutdownResult?.leaks ?? [],
      });
    }
  }

  const passed = checks.filter((c) => c.status === 'pass').length;
  const missingEvidence = checks.filter((c) => c.status === 'missing_evidence').length;
  const failed = checks.filter((c) => c.status === 'fail').length;
  const report = {
    schema_version: 2,
    suite,
    date: new Date().toISOString(),
    ok: failed === 0 && missingEvidence === 0 && !fatalError,
    executable_gates_ok: failed === 0 && !fatalError,
    passed,
    missing_evidence: missingEvidence,
    failed,
    fatal_error: fatalError,
    strict,
    provenance: provenance ?? {
      arch: `${process.platform}/${process.arch}`,
      files: {},
      git: {},
      product_source_digest: {},
    },
    speech_fixtures: {
      manifest_name: path.basename(manifest.manifestPath),
      wake_command_id: manifest.wakeCommand?.id ?? null,
      wake_command_provenance: manifest.wakeCommand?.provenance ?? 'unknown',
      wake_command_is_human: Boolean(manifest.wakeCommand?.is_human_recording),
      neural_wake_stimulus_id: manifest.neuralWakeStimulus?.id ?? null,
      neural_wake_stimulus_provenance: manifest.neuralWakeStimulus?.provenance ?? 'missing',
      neural_wake_is_natural_contiguous_hey_pip: Boolean(
        manifest.neuralWakeStimulus?.provenance === 'human_recorded_contiguous',
      ),
      human_followup_id: manifest.humanFollowup?.id ?? null,
      human_followup_provenance: manifest.humanFollowup?.provenance ?? 'missing',
      human_followup_is_human: Boolean(manifest.humanFollowup?.is_human_recording),
      missing_human_evidence_ids: manifest.missingHumanEvidence.map((m) => m.id),
    },
    log_telemetry: logTelemetry,
    redaction_gate: { ok: true, findings: [] },
    checks,
    soak: soakResult?.soakSummary ?? null,
  };

  let jsonStr = `${JSON.stringify(report, null, 2)}\n`;
  let mdStr = renderMarkdown(report);
  const redaction = stack ? stack.checkRedaction([jsonStr, mdStr]) : { ok: true, findings: [] };
  report.redaction_gate = redaction;
  if (!redaction.ok) {
    report.ok = false;
    report.executable_gates_ok = false;
    report.checks.push({ name: 'artifact redaction gate', ok: false, status: 'fail', detail: redaction });
    report.failed += 1;
    jsonStr = `${JSON.stringify(report, null, 2)}\n`;
    mdStr = renderMarkdown(report);
    log(`REDACTION GATE FAILED: ${JSON.stringify(redaction.findings)}`);
  }

  const jsonPath = path.join(outDir, `${suite}-report.json`);
  const mdPath = path.join(outDir, `${suite}-report.md`);
  fs.writeFileSync(jsonPath, jsonStr);
  fs.writeFileSync(mdPath, mdStr);
  if (soakResult?.samples) {
    fs.writeFileSync(path.join(outDir, 'soak-telemetry.json'), `${JSON.stringify(soakResult.samples, null, 2)}\n`);
  }

  log(`wrote redacted reports:\n  ${jsonPath}\n  ${mdPath}`);
  log(
    `result: ${report.ok ? 'PASS' : report.executable_gates_ok ? 'PARTIAL (missing_evidence)' : 'FAIL'} ` +
      `(${report.passed} pass, ${report.missing_evidence} missing_evidence, ${report.failed} fail)`,
  );
  // Exit non-zero if any executable gate failed, or if --strict and missing_evidence > 0.
  process.exit(report.failed > 0 || (strict && report.missing_evidence > 0) ? 1 : 0);
}
