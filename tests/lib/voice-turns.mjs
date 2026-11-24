// Spoken-turn and barge-in measurements on a stack from ./stack.mjs, shared by the bench
// and the voice eval so both apply the same checks.
//
// With speech faked, a turn fails when it plays no PCM, its player does not exit cleanly,
// or the player received a different number of samples (or sentences) than the fake
// provider sent; a barge-in fails when the barged turn does not end in turn.cancelled or
// its player is not stopped. Failures go to `fail(what, detail)` with expected and
// observed values.
import { silences, sleep, TERMINAL } from './stack.mjs';

const samples = (bytes) => Math.floor(bytes / 2);

export function voiceTurns(stack, { fail, log = () => {} }) {
  const { SPEECH, events, players, speech, model, waitEvent, until, say } = stack;

  /** One spoken turn; returns its timings relative to submit, and checks its audio. */
  async function turn(text, { name }) {
    const firstPlayer = players.length;
    const firstRequest = speech.requests.length;
    const t0 = say(text);
    const started = await waitEvent((e) => e.kind === 'turn.started' && e.payload?.text === text && e.t >= t0, 30000, 'turn.started');
    const id = started.turn_id;
    const done = await waitEvent((e) => e.turn_id === id && TERMINAL.has(e.kind), 60000, 'turn end');
    if (done.kind !== 'turn.completed') fail(`${name}: turn did not complete`, { turn_id: id, expected: 'turn.completed', observed: done.kind });
    let player = null;
    let audio = null;
    if (SPEECH) {
      // A missing player or an unclean end is a failure, not a dropped sample.
      player = await until('player for the turn', async () => players.slice(firstPlayer).find((p) => p.chunks.length), 30000).catch(
        () => null,
      );
      if (player) await until('player exit', async () => player.closed, 60000).catch(() => {});
      const sent = speech.requests.slice(firstRequest);
      audio = {
        expected_samples: samples(sent.reduce((a, q) => a + q.bytes, 0)),
        observed_samples: player ? samples(player.total) : 0,
        speech_requests: sent.length,
        sentences_heard: player ? [...player.tags].filter((tag) => sent.some((q) => q.tag === tag)).length : 0,
        clean_end: Boolean(player?.eof && player?.end && player?.closed),
      };
      if (!player) fail(`${name}: no PCM at the player`, { turn_id: id, ...audio });
      else if (!audio.clean_end) {
        fail(`${name}: player did not end cleanly`, { turn_id: id, eof: Boolean(player.eof), end: Boolean(player.end), ...audio });
      } else if (audio.observed_samples !== audio.expected_samples || audio.sentences_heard !== audio.speech_requests) {
        fail(`${name}: played audio differs from what the provider sent`, { turn_id: id, ...audio });
      }
    }
    const first = (kind) => events.find((e) => e.turn_id === id && e.kind === kind);
    const rel = (t) => (t == null ? null : t - t0);
    return {
      id,
      kind: done.kind,
      started: rel(started.t),
      text: rel(first('text.delta')?.t),
      ready: rel(first('speech.ready')?.t),
      done: rel(done.t),
      pcm: rel(player?.chunks[0]?.[0]),
      // Cold-path breakdown: first speech request at the fake provider, fake aplay started.
      speech_request: rel(speech.requests.find((q) => q.t >= t0)?.t),
      player_start: rel(player?.start),
      gaps: player ? silences(player) : [],
      audio,
    };
  }

  /** A long reply, interrupted by a new utterance once its audio has played 500 ms. */
  async function barge(i, { name }) {
    const firstPlayer = players.length;
    const text = `Barge ${i}: tell me a long story please.`;
    const t0 = say(text);
    const started = await waitEvent((e) => e.kind === 'turn.started' && e.payload?.text === text && e.t >= t0, 30000, 'barge turn');
    if (SPEECH) {
      await until('barge audio', async () => players.slice(firstPlayer).find((p) => p.chunks.length), 30000);
    } else {
      await waitEvent((e) => e.turn_id === started.turn_id && e.kind === 'speech.ready', 30000, 'barge speech.ready');
    }
    await sleep(500);
    const player = players.slice(firstPlayer).find((p) => p.chunks.length);
    const modelOpen = model.open.size > 0;
    const abortedBefore = model.aborted.length;
    const reply = `Stop. Barge reply ${i}.`;
    const tb = say(reply);
    const ended = await waitEvent((e) => e.turn_id === started.turn_id && TERMINAL.has(e.kind), 30000, 'barged turn end');
    if (player) await until('player stopped', async () => player.closed, 10000).catch(() => {});
    await sleep(300);
    const b = {
      end_kind: ended.kind,
      to_terminal_event: ended.t - tb,
      to_audio_closed: player?.closed ? player.closed - tb : null,
      to_provider_closed: modelOpen && model.aborted.length > abortedBefore ? model.aborted[abortedBefore].t - tb : null,
      provider_was_streaming: modelOpen,
      // Stopped: the player exited before playing out what it was sent.
      player_stopped: SPEECH ? Boolean(player?.closed && !player.end) : null,
    };
    if (ended.kind !== 'turn.cancelled') {
      fail(`${name}: barged turn did not end in turn.cancelled`, { expected: 'turn.cancelled', observed: ended.kind });
    }
    if (SPEECH && !b.player_stopped) {
      fail(`${name}: player was not stopped`, { player: Boolean(player), closed: Boolean(player?.closed), played_out: Boolean(player?.end) });
    }
    log(`${name}: ${ended.kind} event ${b.to_terminal_event.toFixed(0)} ms, audio closed ${b.to_audio_closed?.toFixed(0) ?? '-'} ms`);
    // Let the replacement turn finish before the next trial.
    await waitEvent((e) => e.kind === 'turn.started' && e.payload?.text === reply, 30000, 'replacement turn').then((s) =>
      waitEvent((e) => e.turn_id === s.turn_id && TERMINAL.has(e.kind), 60000, 'replacement end'),
    );
    await until('players idle', async () => players.every((p) => p.closed), 30000).catch(() => {});
    await sleep(300);
    return b;
  }

  return { turn, barge };
}
