// Standalone, importable test fixtures for ARM64 Pip E2E and deployment rehearsal:
//   - Ephemeral test CA + localhost/127.0.0.1 leaf certificate (never committed)
//   - TLS-terminating reverse/fault proxy (supports HTTP/1.1 + WebSocket upgrades,
//     holdPushes, holdNext, dropNext, blackhole, cutAll, release)
//   - Mock HTTPS RS256 OIDC IdP (discovery, JWKS, authorize, token with PKCE S256,
//     RS256 ID token with nonce, and Bearer userinfo)
//   - Mock OpenAI-compatible streaming model (SSE content + real Rig express tool_calls)
//   - Mock speech PCM provider (24 kHz mono S16_LE real sine-wave audio, realistic RMS)
//
// Zero private credentials; every key/token is generated per run in the temp dir.
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import tls from 'node:tls';

export const now = () => performance.timeOrigin + performance.now();
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const b64u = (buf) => Buffer.from(buf).toString('base64url');

/**
 * Generates an ephemeral self-signed Test CA and a leaf certificate valid for
 * 127.0.0.1 and localhost, plus a combined CA bundle suitable for SSL_CERT_FILE.
 */
export function createTestTls(dir) {
  const tlsDir = path.join(dir, 'tls');
  fs.mkdirSync(tlsDir, { recursive: true, mode: 0o700 });
  const caKeyPath = path.join(tlsDir, 'ca.key');
  const caCertPath = path.join(tlsDir, 'ca.crt');
  const leafKeyPath = path.join(tlsDir, 'leaf.key');
  const leafCsrPath = path.join(tlsDir, 'leaf.csr');
  const leafCertPath = path.join(tlsDir, 'leaf.crt');
  const leafExtPath = path.join(tlsDir, 'leaf.ext');
  const caExtPath = path.join(tlsDir, 'ca.ext');
  const bundlePath = path.join(tlsDir, 'ca-bundle.pem');

  fs.writeFileSync(
    caExtPath,
    [
      '[req]',
      'distinguished_name = dn',
      'x509_extensions = v3_ca',
      'prompt = no',
      '[dn]',
      'CN = Pip E2E Ephemeral Test CA',
      '[v3_ca]',
      'basicConstraints = critical, CA:TRUE, pathlen:0',
      'keyUsage = critical, keyCertSign, cRLSign',
      'subjectKeyIdentifier = hash',
    ].join('\n'),
  );
  fs.writeFileSync(
    leafExtPath,
    [
      'basicConstraints = critical, CA:FALSE',
      'keyUsage = critical, digitalSignature, keyEncipherment',
      'extendedKeyUsage = serverAuth',
      'subjectAltName = IP:127.0.0.1,DNS:localhost',
    ].join('\n'),
  );

  const runSsl = (args) => {
    const r = spawnSync('openssl', args, { encoding: 'utf8' });
    if (r.status !== 0) {
      throw new Error(`openssl ${args[0]} failed (exit ${r.status}): ${r.stderr}`);
    }
  };

  runSsl([
    'req',
    '-x509',
    '-newkey',
    'ec',
    '-pkeyopt',
    'ec_paramgen_curve:prime256v1',
    '-nodes',
    '-days',
    '2',
    '-keyout',
    caKeyPath,
    '-out',
    caCertPath,
    '-config',
    caExtPath,
  ]);
  runSsl([
    'req',
    '-new',
    '-newkey',
    'ec',
    '-pkeyopt',
    'ec_paramgen_curve:prime256v1',
    '-nodes',
    '-keyout',
    leafKeyPath,
    '-out',
    leafCsrPath,
    '-subj',
    '/CN=127.0.0.1',
  ]);
  runSsl([
    'x509',
    '-req',
    '-in',
    leafCsrPath,
    '-CA',
    caCertPath,
    '-CAkey',
    caKeyPath,
    '-CAcreateserial',
    '-out',
    leafCertPath,
    '-days',
    '2',
    '-extfile',
    leafExtPath,
  ]);

  const caPem = fs.readFileSync(caCertPath, 'utf8');
  const sysBundleCandidates = [
    '/etc/ssl/certs/ca-certificates.crt',
    '/etc/ssl/cert.pem',
  ];
  const sysPem = sysBundleCandidates.find((p) => fs.existsSync(p));
  fs.writeFileSync(
    bundlePath,
    sysPem ? `${fs.readFileSync(sysPem, 'utf8')}\n${caPem}` : caPem,
    { mode: 0o644 },
  );

  return {
    dir: tlsDir,
    caCertPath,
    caKeyPath,
    leafCertPath,
    leafKeyPath,
    bundlePath,
    key: fs.readFileSync(leafKeyPath, 'utf8'),
    cert: fs.readFileSync(leafCertPath, 'utf8'),
    ca: caPem,
  };
}

/**
 * TLS-terminating reverse + fault proxy for HTTPS and WSS connections.
 * Forces ALPN http/1.1 so HTTP requests and WebSocket frames are cleanly inspectable
 * after TLS termination.
 */
export async function startTlsFaultProxy({
  listen = 0,
  upstreamHost = '127.0.0.1',
  upstreamPort,
  key,
  cert,
  name = 'proxy',
}) {
  const conns = new Set();
  const seen = [];
  const state = {
    pushes: false,
    hold: null,
    drop: null,
    blackhole: false,
    upstreamPort,
  };

  function* clientFrames(conn) {
    for (;;) {
      const b = conn.frames;
      if (!b || b.length < 2) return;
      let len = b[1] & 0x7f;
      let at = 2;
      if (len === 126) {
        if (b.length < 4) return;
        len = b.readUInt16BE(2);
        at = 4;
      } else if (len === 127) {
        if (b.length < 10) return;
        len = Number(b.readBigUInt64BE(2));
        at = 10;
      }
      const masked = (b[1] & 0x80) !== 0;
      const maskKey = masked ? b.subarray(at, at + 4) : null;
      if (masked) at += 4;
      if (b.length < at + len) return;
      const payload = Buffer.from(b.subarray(at, at + len));
      if (maskKey) for (let i = 0; i < payload.length; i++) payload[i] ^= maskKey[i & 3];
      conn.frames = b.subarray(at + len);
      yield payload;
    }
  }

  const server = tls.createServer(
    {
      key,
      cert,
      ALPNProtocols: ['http/1.1'],
    },
    (client) => {
      if (state.blackhole) {
        seen.push({ t: now(), proxy: name, fault: 'blackhole_reject' });
        client.destroy();
        return;
      }
      const up = net.createConnection({ host: upstreamHost, port: state.upstreamPort });
      const conn = {
        ws: null,
        held: false,
        queue: [],
        drop: false,
        tail: Buffer.alloc(0),
        frames: null,
        client,
        up,
      };
      conn.flush = () => {
        conn.held = false;
        for (const chunk of conn.queue.splice(0)) {
          if (!client.destroyed) client.write(chunk);
        }
      };
      conn.destroy = () => {
        client.destroy();
        up.destroy();
      };
      conns.add(conn);
      const done = () => {
        conns.delete(conn);
        client.destroy();
        up.destroy();
      };
      client.on('close', done);
      up.on('close', done);
      client.on('error', done);
      up.on('error', done);

      const match = (bytes) => {
        if (state.hold && bytes.includes(state.hold.marker)) {
          conn.held = true;
          seen.push({ t: now(), proxy: name, fault: 'hold', marker: state.hold.marker.toString() });
          const { resolve } = state.hold;
          state.hold = null;
          resolve(now());
        } else if (state.drop && bytes.includes(state.drop.marker)) {
          conn.drop = true;
          seen.push({ t: now(), proxy: name, fault: 'drop', marker: state.drop.marker.toString() });
          const { resolve } = state.drop;
          state.drop = null;
          resolve(now());
        }
      };

      client.on('data', (chunk) => {
        if (state.blackhole) {
          conn.destroy();
          return;
        }
        if (conn.ws === null) {
          const text = chunk.toString('latin1');
          conn.ws = /upgrade:\s*websocket/i.test(text);
          if (conn.ws) {
            seen.push({ t: now(), proxy: name, fault: null, event: 'websocket' });
            const end = text.indexOf('\r\n\r\n');
            conn.frames = end >= 0 ? chunk.subarray(end + 4) : Buffer.alloc(0);
            for (const payload of clientFrames(conn)) match(payload);
          } else {
            seen.push({ t: now(), proxy: name, fault: null, event: 'http' });
          }
        } else if (conn.ws) {
          conn.frames = Buffer.concat([conn.frames, chunk]);
          for (const payload of clientFrames(conn)) match(payload);
        }
        if (!conn.ws) {
          match(Buffer.concat([conn.tail, chunk]));
          conn.tail = chunk.subarray(Math.max(0, chunk.length - 64));
        }
        if (!up.destroyed) up.write(chunk);
      });

      up.on('data', (chunk) => {
        if (state.blackhole || conn.drop) {
          conn.destroy();
          return;
        }
        if (conn.held || (conn.ws && state.pushes)) {
          conn.queue.push(chunk);
          return;
        }
        if (!client.destroyed) client.write(chunk);
      });
    },
  );

  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(listen, '127.0.0.1', resolve);
  });
  const port = server.address().port;

  return {
    port,
    url: `https://127.0.0.1:${port}`,
    seen,
    activeConnections: () => conns.size,
    websocketCount: () => seen.filter((s) => s.event === 'websocket').length,
    setUpstreamPort(newPort) {
      state.upstreamPort = newPort;
    },
    holdPushes() {
      state.pushes = true;
    },
    holdNext(marker) {
      return new Promise((resolve) => {
        state.hold = { marker: Buffer.from(marker), resolve };
      });
    },
    dropNext(marker) {
      return new Promise((resolve) => {
        state.drop = { marker: Buffer.from(marker), resolve };
      });
    },
    setBlackhole(enabled) {
      state.blackhole = Boolean(enabled);
      seen.push({ t: now(), proxy: name, fault: enabled ? 'blackhole_on' : 'blackhole_off' });
      if (enabled) {
        for (const c of [...conns]) c.destroy();
      }
    },
    cutAll() {
      seen.push({ t: now(), proxy: name, fault: 'cut_all', count: conns.size });
      for (const c of [...conns]) c.destroy();
    },
    release() {
      state.pushes = false;
      state.hold = null;
      state.blackhole = false;
      for (const c of conns) c.flush();
    },
    close() {
      for (const c of [...conns]) c.destroy();
      server.close();
    },
  };
}

/**
 * Mock HTTPS RS256 OIDC Identity Provider compatible with apps/server/src/oidc.rs
 * and apps/server/src/claim.rs:
 *   - GET  /.well-known/openid-configuration
 *   - GET  /jwks
 *   - GET  /authorize (auto-approves and redirects with code & state)
 *   - POST /token     (verifies client_id, client_secret, redirect_uri, PKCE S256)
 *   - GET  /userinfo  (verifies Bearer access_token, returns {"sub": subject})
 */
export function startMockOidc({
  port = 0,
  key,
  cert,
  clientId = 'pip-e2e-client',
  clientSecret = 'pip-e2e-oidc-secret-not-real-0123456789',
  subject = 'pip-e2e-owner-1',
}) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', {
    modulusLength: 2048,
  });
  const kid = `pip-e2e-${crypto.randomBytes(6).toString('hex')}`;
  const pubJwk = {
    ...publicKey.export({ format: 'jwk' }),
    kid,
    use: 'sig',
    alg: 'RS256',
    key_ops: ['verify'],
  };

  const codes = new Map();
  const accessTokens = new Map();
  const audit = {
    discoveries: 0,
    jwksFetches: 0,
    authorizes: 0,
    tokens: 0,
    userinfos: 0,
  };

  let issuer = '';

  const signJwt = (payload) => {
    const header = { alg: 'RS256', typ: 'JWT', kid };
    const headB64 = b64u(JSON.stringify(header));
    const bodyB64 = b64u(JSON.stringify(payload));
    const signingInput = `${headB64}.${bodyB64}`;
    const sig = crypto.sign('RSA-SHA256', Buffer.from(signingInput), privateKey);
    return `${signingInput}.${b64u(sig)}`;
  };

  const server = Bun.serve({
    hostname: '127.0.0.1',
    port,
    tls: { key, cert },
    async fetch(req) {
      const url = new URL(req.url);
      if (req.method === 'GET' && url.pathname === '/.well-known/openid-configuration') {
        audit.discoveries += 1;
        return Response.json({
          issuer,
          authorization_endpoint: `${issuer}/authorize`,
          token_endpoint: `${issuer}/token`,
          userinfo_endpoint: `${issuer}/userinfo`,
          jwks_uri: `${issuer}/jwks`,
          response_types_supported: ['code'],
          subject_types_supported: ['public'],
          id_token_signing_alg_values_supported: ['RS256'],
        });
      }
      if (req.method === 'GET' && url.pathname === '/jwks') {
        audit.jwksFetches += 1;
        return Response.json({ keys: [pubJwk] });
      }
      if (req.method === 'GET' && url.pathname === '/authorize') {
        audit.authorizes += 1;
        const reqClientId = url.searchParams.get('client_id');
        const redirectUri = url.searchParams.get('redirect_uri');
        const state = url.searchParams.get('state');
        const nonce = url.searchParams.get('nonce');
        const codeChallenge = url.searchParams.get('code_challenge');
        const method = url.searchParams.get('code_challenge_method');
        if (reqClientId !== clientId || !redirectUri || !state || !nonce || method !== 'S256' || !codeChallenge) {
          return new Response('invalid authorize parameters', { status: 400 });
        }
        const code = `code-${crypto.randomBytes(16).toString('hex')}`;
        codes.set(code, {
          redirectUri,
          nonce,
          codeChallenge,
          used: false,
        });
        const target = new URL(redirectUri);
        target.searchParams.set('code', code);
        target.searchParams.set('state', state);
        return Response.redirect(target.toString(), 302);
      }
      if (req.method === 'POST' && url.pathname === '/token') {
        audit.tokens += 1;
        const form = new URLSearchParams(await req.text());
        if (form.get('grant_type') !== 'authorization_code') {
          return new Response('unsupported grant_type', { status: 400 });
        }
        if (form.get('client_id') !== clientId) {
          return new Response('invalid client_id', { status: 401 });
        }
        if (clientSecret && form.get('client_secret') !== clientSecret) {
          return new Response('invalid client_secret', { status: 401 });
        }
        const code = form.get('code') ?? '';
        const entry = codes.get(code);
        if (!entry || entry.used) {
          return new Response('invalid code', { status: 400 });
        }
        if (form.get('redirect_uri') !== entry.redirectUri) {
          return new Response('redirect_uri mismatch', { status: 400 });
        }
        const verifier = form.get('code_verifier') ?? '';
        const expectedChallenge = b64u(crypto.createHash('sha256').update(verifier).digest());
        if (expectedChallenge !== entry.codeChallenge) {
          return new Response('PKCE S256 verification failed', { status: 400 });
        }
        entry.used = true;
        const accessToken = `at-${crypto.randomBytes(16).toString('hex')}`;
        accessTokens.set(accessToken, { sub: subject });
        const iat = Math.floor(Date.now() / 1000);
        const idToken = signJwt({
          iss: issuer,
          sub: subject,
          aud: clientId,
          nonce: entry.nonce,
          iat,
          auth_time: iat,
          exp: iat + 300,
        });
        return Response.json({
          access_token: accessToken,
          token_type: 'Bearer',
          expires_in: 300,
          id_token: idToken,
        });
      }
      if (req.method === 'GET' && url.pathname === '/userinfo') {
        audit.userinfos += 1;
        const authHeader = req.headers.get('authorization') ?? '';
        const token = authHeader.startsWith('Bearer ') ? authHeader.slice(7) : '';
        const info = accessTokens.get(token);
        if (!info) return new Response('unauthorized', { status: 401 });
        return Response.json({ sub: info.sub, name: 'Pip E2E Owner' });
      }
      return new Response('not found', { status: 404 });
    },
  });

  issuer = `https://127.0.0.1:${server.port}`;

  return {
    port: server.port,
    issuer,
    clientId,
    clientSecret,
    subject,
    expectedOwner: `${issuer}|${subject}`,
    audit,
    stop() {
      server.stop(true);
    },
  };
}

/**
 * Mock OpenAI-compatible streaming chat completions server.
 * Supports:
 *   - Plain streamed dialogue
 *   - Real Rig 0.46.0 tool_calls (e.g. `express` tool -> followed by spoken reply)
 *   - Per-call script hooks via `setScript(fn)`
 *
 * Script hook signature:
 *   (ctx) => { kind: 'text', text, chunkMs?, firstTokenMs? }
 *          | { kind: 'tool', id?, name, args }
 *          | null (use default)
 * where `ctx` = { messages, lastUserText, hasTools, toolResultCount, callIndex }.
 */
export function startMockModel({ port = 0, script = null } = {}) {
  const stats = {
    calls: 0,
    open: new Map(),
    aborted: [],
    streams: [],
    toolCalls: [],
    userTexts: [],
  };
  let activeScript = script;

  const chunkFrame = (delta, finish) => {
    const chunk = {
      id: 'chatcmpl-pip-e2e',
      object: 'chat.completion.chunk',
      created: Math.floor(Date.now() / 1000),
      model: 'pip-e2e-fixture',
      choices: [{ index: 0, delta, finish_reason: finish ?? null }],
    };
    if (finish) {
      chunk.usage = { prompt_tokens: 12, completion_tokens: 8, total_tokens: 20 };
    }
    return `data: ${JSON.stringify(chunk)}\n\n`;
  };

  const extractText = (msg) => {
    if (!msg) return '';
    if (typeof msg.content === 'string') return msg.content;
    if (Array.isArray(msg.content)) {
      return msg.content
        .map((part) => (typeof part === 'string' ? part : part?.text ?? ''))
        .join(' ');
    }
    return JSON.stringify(msg.content ?? '');
  };

  const server = Bun.serve({
    hostname: '127.0.0.1',
    port,
    idleTimeout: 120,
    async fetch(req) {
      if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/chat/completions')) {
        return new Response('not found', { status: 404 });
      }
      const body = await req.json();
      stats.calls += 1;
      const callIndex = stats.calls;
      const messages = Array.isArray(body.messages) ? body.messages : [];
      const lastUser = [...messages].reverse().find((m) => m.role === 'user');
      const lastUserText = extractText(lastUser);
      const hasTools = Array.isArray(body.tools) && body.tools.length > 0;
      const toolResultCount = messages.filter((m) => m.role === 'tool').length;

      if (toolResultCount === 0 && hasTools) {
        stats.userTexts.push({ t: now(), callIndex, text: lastUserText });
      } else if (!hasTools && toolResultCount === 0) {
        stats.userTexts.push({ t: now(), callIndex, text: lastUserText, spokenPhase: true });
      }

      const ctx = { messages, lastUserText, hasTools, toolResultCount, callIndex };
      let plan = activeScript?.(ctx) ?? null;
      if (!plan) {
        plan = {
          kind: 'text',
          text: `Heard you loud and clear on turn ${callIndex}. Everything is running smoothly.`,
        };
      }

      if (plan.kind === 'tool') {
        const toolId = plan.id ?? `call_${callIndex}`;
        const toolArgs = typeof plan.args === 'string' ? plan.args : JSON.stringify(plan.args ?? {});
        stats.toolCalls.push({ t: now(), callIndex, id: toolId, name: plan.name, args: plan.args });
        const delta = {
          role: 'assistant',
          tool_calls: [
            {
              index: 0,
              id: toolId,
              type: 'function',
              function: { name: plan.name, arguments: toolArgs },
            },
          ],
        };
        const sse = `${chunkFrame(delta, 'tool_calls')}data: [DONE]\n\n`;
        return new Response(sse, { headers: { 'content-type': 'text/event-stream' } });
      }

      const reply = String(plan.text ?? 'Okay.');
      if (!body.stream) {
        return Response.json({
          id: 'chatcmpl-pip-e2e',
          object: 'chat.completion',
          created: Math.floor(Date.now() / 1000),
          model: 'pip-e2e-fixture',
          choices: [{ index: 0, message: { role: 'assistant', content: reply }, finish_reason: 'stop' }],
          usage: { prompt_tokens: 12, completion_tokens: 8, total_tokens: 20 },
        });
      }

      const firstTokenMs = plan.firstTokenMs ?? 40;
      const chunkMs = plan.chunkMs ?? 10;
      const frames = [];
      for (let i = 0; i < reply.length; i += 6) {
        frames.push(chunkFrame({ role: 'assistant', content: reply.slice(i, i + 6) }, null));
      }
      frames.push(`${chunkFrame({}, 'stop')}data: [DONE]\n\n`);

      const start = now();
      const encoder = new TextEncoder();
      let k = 0;
      let cancelled = false;
      stats.open.set(callIndex, { start, reply, lastUserText });
      stats.streams.push({ id: callIndex, start, reply, lastUserText, hasTools, toolResultCount });

      const stream = new ReadableStream({
        async pull(controller) {
          const wait = start + firstTokenMs + chunkMs * k - now();
          if (wait > 0) await sleep(wait);
          if (cancelled) return;
          controller.enqueue(encoder.encode(frames[k++]));
          if (k === frames.length) {
            stats.open.delete(callIndex);
            controller.close();
          }
        },
        cancel() {
          cancelled = true;
          stats.open.delete(callIndex);
          stats.aborted.push({ id: callIndex, t: now() });
        },
      });
      return new Response(stream, { headers: { 'content-type': 'text/event-stream' } });
    },
  });

  return {
    port: server.port,
    baseUrl: `http://127.0.0.1:${server.port}/v1`,
    stats,
    setScript(fn) {
      activeScript = fn;
    },
    stop() {
      server.stop(true);
    },
  };
}

/**
 * Mock speech PCM provider matching OpenRouter elevenlabs/eleven-v4-turbo contract:
 *   POST /v1/audio/speech -> audio/pcm (24 kHz mono S16_LE)
 *
 * Emits a real non-zero sine wave (440..880 Hz) instead of constant DC bytes so
 * resampling 24 kHz -> 16 kHz in virtual echo loopback produces realistic acoustic energy.
 */
export function startMockSpeech({
  port = 0,
  apiKey = 'pip-e2e-speech-key-not-real',
  firstByteMs = 80,
  msPerChar = 35,
  realtime = true,
  amplitude = 9000,
} = {}) {
  const SAMPLE_RATE = 24000;
  const BYTES_PER_MS = (SAMPLE_RATE * 2) / 1000; // 48 bytes/ms
  const CHUNK_MS = 20;
  const stats = { requests: [], aborted: [] };
  const config = { firstByteMs, msPerChar, realtime, amplitude };

  // Emit a gentle 180 Hz single-frequency modulated tone (avoiding dual 440+880 Hz harmonics
  // that can falsely trigger openWakeWord hey_pip.onnx during echo-loopback tests).
  const makeSineChunk = (sampleStart, sampleCount, freqHz, amp) => {
    const buf = Buffer.allocUnsafe(sampleCount * 2);
    const omega = (2 * Math.PI * freqHz) / SAMPLE_RATE;
    const modOmega = (2 * Math.PI * 3.5) / SAMPLE_RATE;
    for (let i = 0; i < sampleCount; i++) {
      const idx = sampleStart + i;
      const env = 0.65 + 0.35 * Math.sin(modOmega * idx);
      const sample = Math.round(amp * env * Math.sin(omega * idx));
      buf.writeInt16LE(Math.max(-32768, Math.min(32767, sample)), i * 2);
    }
    return buf;
  };

  const server = Bun.serve({
    hostname: '127.0.0.1',
    port,
    idleTimeout: 120,
    async fetch(req) {
      if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/audio/speech')) {
        return new Response('not found', { status: 404 });
      }
      if (req.headers.get('authorization') !== `Bearer ${apiKey}`) {
        return new Response('unauthorized', { status: 401 });
      }
      const body = await req.json();
      const t = now();
      const input = String(body.input ?? '');
      const durationMs = Math.max(CHUNK_MS * 4, Math.min(20_000, input.length * config.msPerChar));
      const totalSamples = Math.round((durationMs * SAMPLE_RATE) / 1000);
      const totalBytes = totalSamples * 2;
      const seq = stats.requests.length + 1;
      const freqHz = 160 + ((seq * 17) % 90);
      const record = {
        seq,
        t,
        input,
        chars: input.length,
        durationMs,
        bytes: totalBytes,
        freqHz,
        format: body.response_format,
        voice: body.voice,
      };
      stats.requests.push(record);

      const waitHead = config.firstByteMs - (now() - t);
      if (waitHead > 0) await sleep(waitHead);

      const start = now();
      const samplesPerChunk = Math.round((CHUNK_MS * SAMPLE_RATE) / 1000);
      let sentSamples = 0;
      let k = 0;
      let cancelled = false;
      const stream = new ReadableStream({
        async pull(controller) {
          if (config.realtime) {
            const wait = start + CHUNK_MS * k - now();
            if (wait > 0) await sleep(wait);
          }
          if (cancelled) return;
          const count = Math.min(samplesPerChunk, totalSamples - sentSamples);
          controller.enqueue(makeSineChunk(sentSamples, count, freqHz, config.amplitude));
          sentSamples += count;
          k += 1;
          if (sentSamples >= totalSamples) {
            record.finishedAt = now();
            controller.close();
          }
        },
        cancel() {
          cancelled = true;
          stats.aborted.push({ t: now(), seq, input });
        },
      });
      return new Response(stream, {
        headers: { 'content-type': 'audio/pcm' },
      });
    },
  });

  return {
    port: server.port,
    baseUrl: `http://127.0.0.1:${server.port}/v1`,
    apiKey,
    bytesPerMs: BYTES_PER_MS,
    config,
    stats,
    stop() {
      server.stop(true);
    },
  };
}
