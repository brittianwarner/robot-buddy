// Deterministic browser test of the real lab page against a mocked lab API: a reply that
// completes while the lab is disconnected is not spoken on reconnect; Replay still is.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const dir = new URL('../src/lab/', import.meta.url).pathname;
const events = [];
const spoken = [];
let failing = false;
let snapshot = [];
const event = (kind, turn_id, payload = {}) =>
  events.push({ sequence: events.length + 1, kind, turn_id, payload });
const reply = (body) => Response.json(body);
const commands = {
  snapshot: () => ({ turns: snapshot, last_sequence: events.length }),
  replay: ({ after }) => events.filter((e) => e.sequence > after).slice(0, 50),
  submit: ({ text }) => {
    const id = 'turn-' + text;
    snapshot = [{ id, text, answer: '', status: 'queued' }, ...snapshot];
    return { turn_id: id, duplicate: false };
  },
  get_voice_config: () => ({ revision: 1, name: 'Will', config: { delivery: '' }, choices: [] }),
  get_context_config: () => ({ revision: 1 }),
  list_personas: () => [],
  get_system_prompt: () => ({ prompt: 'Buddy', revision: 1 }),
};
const assets = { '/lab/app.js': 'app.js', '/lab/voice.js': 'voice.js', '/lab/style.css': 'style.css' };
const server = Bun.serve({
  port: 0,
  idleTimeout: 60,
  fetch: async (req) => {
    const path = new URL(req.url).pathname;
    if (path === '/lab') return new Response(Bun.file(dir + 'index.html'), { headers: { 'content-type': 'text/html' } });
    if (assets[path]) return new Response(Bun.file(dir + assets[path]));
    if (path === '/lab/api/state')
      return failing
        ? new Response(null, { status: 503 })
        : reply({ csrf: 'test', provider: 'test', model: 'test', speech_model: 'test', oidc_configured: true, bot: { bot_id: 'bot-1', owner: 'owner' } });
    if (path === '/lab/api/action') {
      if (failing) return new Response(null, { status: 503 });
      const { target, command } = await req.json();
      if (target === 'memory') return reply([]);
      return reply(commands[command.type]?.(command) ?? {});
    }
    if (path === '/lab/api/speech') {
      const { turn_id } = await req.json();
      spoken.push(turn_id);
      return new Response(new Uint8Array(8820), {
        headers: { 'content-type': 'audio/pcm;rate=44100;channels=1', 'x-audio-turn-id': turn_id, 'x-audio-encoding': 'pcm_s16le' },
      });
    }
    return new Response(null, { status: 404 });
  },
});

const complete = (id, answer) => {
  event('speech.ready', id, { index: 0 });
  event('turn.completed', id, { text: answer });
  snapshot = snapshot.map((t) => (t.id === id ? { ...t, status: 'completed', answer, speech_ready: true } : t));
};
const until = async (what, check) => {
  for (let i = 0; i < 200; i++) {
    if (await check()) return;
    await new Promise((r) => setTimeout(r, 25));
  }
  throw new Error('timed out: ' + what);
};

const browser = await chromium.launch({ headless: true });
try {
  const page = await browser.newPage();
  await page.goto(server.url + 'lab');
  const connection = () => page.locator('#connection').textContent();
  await until('connected', async () => (await connection()) === 'Connected · durable replay');
  await page.locator('#voice-enabled').check();
  const send = async (text) => {
    await until('send enabled', () => page.locator('#send').isEnabled());
    await page.locator('#message').fill(text);
    await page.locator('#send').click();
    await until('submitted', () => snapshot.some((t) => t.id === 'turn-' + text));
  };

  // Control: a reply that completes while connected is spoken.
  await send('live');
  complete('turn-live', 'Hello.');
  await until('live reply spoken', () => spoken.includes('turn-live'));

  // A reply that completes during an outage is not spoken when catch-up arrives.
  await send('outage');
  failing = true;
  await until('disconnected', async () => (await connection()) === 'Reconnecting…');
  complete('turn-outage', 'Missed you.');
  failing = false;
  await until('reconnected', async () => (await connection()) === 'Connected · durable replay');
  await until('catch-up applied', async () => (await page.locator('#turn-status').textContent()).startsWith('Enter to send'));
  await new Promise((r) => setTimeout(r, 300));
  assert.deepEqual(spoken, ['turn-live'], 'catch-up must not start audio');

  // Replay stays available for the reply that was missed.
  await page.locator('#voice-replay').click();
  await until('replayed', () => spoken.includes('turn-outage'));
  assert.deepEqual(spoken, ['turn-live', 'turn-outage']);
  console.log('PASS: catch-up after reconnect is silent; Replay speaks the missed reply');
} finally {
  await browser.close();
  server.stop(true);
}
