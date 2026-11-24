#!/usr/bin/env bun
// Isolated backend fixture for the fresh ARM64 Pi deployment rehearsal.
//
// Runs inside the dedicated `pip-qa-backend` ARM64 container (separate from the Pi
// systemd guest, per DEPLOYMENT.md §1):
//   1. Generates a disposable per-run EC P-256 Test CA + leaf certificate for
//      backend.pip-qa.internal, rivet.pip-qa.internal, oidc.pip-qa.internal.
//   2. Runs the real `robot-buddy-server init` + `robot-buddy-server provision`
//      commands (DEPLOYMENT.md §1), exporting ONLY the public CA cert and the
//      single device identity JSON to `/shared/`.
//   3. Runs a real RS256 mock HTTPS OIDC IdP (`https://oidc.pip-qa.internal:9443`):
//      discovery, JWKS, /authorize (normal + optional error=access_denied simulation),
//      /token (verifies client_id, client_secret, redirect_uri, PKCE S256, signs RS256
//      ID token with nonce/iss/sub/aud/iat/exp), and /userinfo (Bearer access_token).
//      (Protocol-aligned with tests/pip-e2e/runner-fakes.mjs, bound to 0.0.0.0 so the
//      separate Pi container connects across the isolated Docker bridge network.)
//   4. Runs a mock OpenAI-compatible streaming model (`http://127.0.0.1:19091/v1`).
//   5. Runs the REAL `robot-buddy-server` + pinned `rivet-engine` 3.1.1 over HTTPS
//      (no BUDDY_ALLOW_INSECURE_LOCAL) behind TLS-terminating reverse proxies on
//      0.0.0.0:8443 (-> 127.0.0.1:8080) and 0.0.0.0:6443 (-> 127.0.0.1:6420, HTTP+WSS).
//   6. Exposes an internal inspection API on 0.0.0.0:19999 for the host rehearsal runner.

import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import tls from 'node:tls';

const b64u = (buf) => Buffer.from(buf).toString('base64url');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const SHARED_DIR = process.env.QA_SHARED_DIR ?? '/shared';
const SECRETS_DIR = process.env.QA_SECRETS_DIR ?? '/tmp/qa-backend-secrets';
const DATA_DIR = process.env.BUDDY_DATA_DIR ?? '/data';
const BACKEND_HOST = process.env.QA_BACKEND_HOST ?? 'backend.pip-qa.internal';
const RIVET_HOST = process.env.QA_RIVET_HOST ?? 'rivet.pip-qa.internal';
const OIDC_HOST = process.env.QA_OIDC_HOST ?? 'oidc.pip-qa.internal';
const BACKEND_TLS_PORT = Number(process.env.QA_BACKEND_TLS_PORT ?? 8443);
const RIVET_TLS_PORT = Number(process.env.QA_RIVET_TLS_PORT ?? 6443);
const OIDC_TLS_PORT = Number(process.env.QA_OIDC_TLS_PORT ?? 9443);
const CONTROL_PORT = Number(process.env.QA_CONTROL_PORT ?? 19999);
const MODEL_PORT = 19091;

const PUBLIC_URL = `https://${BACKEND_HOST}:${BACKEND_TLS_PORT}`;
const PUBLIC_RIVET_URL = `https://${RIVET_HOST}:${RIVET_TLS_PORT}`;
const OIDC_ISSUER = `https://${OIDC_HOST}:${OIDC_TLS_PORT}`;
const OIDC_CLIENT_ID = 'pip-deploy-qa-client';
const OIDC_CLIENT_SECRET = 'pip-deploy-qa-oidc-secret-not-real-0123456789';
const OIDC_SUBJECT = 'pip-deploy-qa-owner-01';

fs.mkdirSync(SHARED_DIR, { recursive: true, mode: 0o755 });
fs.mkdirSync(SECRETS_DIR, { recursive: true, mode: 0o700 });
fs.mkdirSync(DATA_DIR, { recursive: true, mode: 0o700 });

// -----------------------------------------------------------------------------
// 1. Disposable Test CA + Leaf Certificate
// -----------------------------------------------------------------------------
function generateTls(dir) {
  const tlsDir = path.join(dir, 'tls');
  fs.mkdirSync(tlsDir, { recursive: true, mode: 0o700 });
  const caKey = path.join(tlsDir, 'ca.key');
  const caCert = path.join(tlsDir, 'ca.crt');
  const leafKey = path.join(tlsDir, 'leaf.key');
  const leafCsr = path.join(tlsDir, 'leaf.csr');
  const leafCert = path.join(tlsDir, 'leaf.crt');
  const caExt = path.join(tlsDir, 'ca.ext');
  const leafExt = path.join(tlsDir, 'leaf.ext');
  const bundle = path.join(tlsDir, 'ca-bundle.pem');

  fs.writeFileSync(
    caExt,
    [
      '[req]',
      'distinguished_name = dn',
      'x509_extensions = v3_ca',
      'prompt = no',
      '[dn]',
      'CN = Pip Deploy QA Ephemeral Test CA',
      '[v3_ca]',
      'basicConstraints = critical, CA:TRUE, pathlen:0',
      'keyUsage = critical, keyCertSign, cRLSign',
      'subjectKeyIdentifier = hash',
    ].join('\n'),
  );
  fs.writeFileSync(
    leafExt,
    [
      'basicConstraints = critical, CA:FALSE',
      'keyUsage = critical, digitalSignature, keyEncipherment',
      'extendedKeyUsage = serverAuth',
      `subjectAltName = DNS:${BACKEND_HOST},DNS:${RIVET_HOST},DNS:${OIDC_HOST},DNS:localhost,IP:127.0.0.1`,
    ].join('\n'),
  );

  const ssl = (args) => {
    const r = spawnSync('openssl', args, { encoding: 'utf8' });
    if (r.status !== 0) {
      throw new Error(`openssl ${args[0]} failed: ${r.stderr}`);
    }
  };
  ssl([
    'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
    '-nodes', '-days', '2', '-keyout', caKey, '-out', caCert, '-config', caExt,
  ]);
  ssl([
    'req', '-new', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1',
    '-nodes', '-keyout', leafKey, '-out', leafCsr, '-subj', `/CN=${BACKEND_HOST}`,
  ]);
  ssl([
    'x509', '-req', '-in', leafCsr, '-CA', caCert, '-CAkey', caKey,
    '-CAcreateserial', '-out', leafCert, '-days', '2', '-extfile', leafExt,
  ]);

  const caPem = fs.readFileSync(caCert, 'utf8');
  const sysBundle = '/etc/ssl/certs/ca-certificates.crt';
  fs.writeFileSync(
    bundle,
    fs.existsSync(sysBundle) ? `${fs.readFileSync(sysBundle, 'utf8')}\n${caPem}` : caPem,
    { mode: 0o644 },
  );
  // Export ONLY the public CA certificate to /shared (never the CA private key).
  fs.writeFileSync(path.join(SHARED_DIR, 'ca.crt'), caPem, { mode: 0o644 });

  return {
    key: fs.readFileSync(leafKey, 'utf8'),
    cert: fs.readFileSync(leafCert, 'utf8'),
    bundlePath: bundle,
  };
}

// -----------------------------------------------------------------------------
// 2. Provision backend secrets + one device identity (DEPLOYMENT.md §1)
// -----------------------------------------------------------------------------
function provisionIdentity() {
  const initOut = spawnSync('/usr/local/bin/robot-buddy-server', ['init', SECRETS_DIR], {
    encoding: 'utf8',
  });
  if (initOut.status !== 0) {
    throw new Error(`robot-buddy-server init failed (${initOut.status}): ${initOut.stderr}`);
  }
  const identityPath = path.join(SHARED_DIR, 'pip-01.identity.json');
  const provOut = spawnSync(
    '/usr/local/bin/robot-buddy-server',
    ['provision', path.join(SECRETS_DIR, 'issuer.key'), identityPath],
    { encoding: 'utf8' },
  );
  if (provOut.status !== 0) {
    throw new Error(`robot-buddy-server provision failed (${provOut.status}): ${provOut.stderr}`);
  }
  fs.chmodSync(identityPath, 0o600);

  const secretsEnv = {};
  for (const line of fs.readFileSync(path.join(SECRETS_DIR, 'secrets.env'), 'utf8').split('\n')) {
    const t = line.trim();
    if (!t || t.startsWith('#')) continue;
    const eq = t.indexOf('=');
    if (eq > 0) secretsEnv[t.slice(0, eq)] = t.slice(eq + 1);
  }
  const identity = JSON.parse(fs.readFileSync(identityPath, 'utf8'));
  return { secretsEnv, deviceId: identity.certificate.device_id };
}

// -----------------------------------------------------------------------------
// 3. Mock HTTPS RS256 OIDC IdP
// -----------------------------------------------------------------------------
function startMockOidc(tlsMaterial) {
  const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
  const kid = `pip-deploy-qa-${crypto.randomBytes(6).toString('hex')}`;
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
    issuer: OIDC_ISSUER,
    clientId: OIDC_CLIENT_ID,
    subject: OIDC_SUBJECT,
    expectedOwner: `${OIDC_ISSUER}|${OIDC_SUBJECT}`,
    discoveries: 0,
    jwksFetches: 0,
    authorizes: 0,
    deniedAuthorizes: 0,
    tokens: 0,
    userinfos: 0,
  };

  const signJwt = (payload) => {
    const headB64 = b64u(JSON.stringify({ alg: 'RS256', typ: 'JWT', kid }));
    const bodyB64 = b64u(JSON.stringify(payload));
    const input = `${headB64}.${bodyB64}`;
    const sig = crypto.sign('RSA-SHA256', Buffer.from(input), privateKey);
    return `${input}.${b64u(sig)}`;
  };

  const server = Bun.serve({
    hostname: '0.0.0.0',
    port: OIDC_TLS_PORT,
    tls: { key: tlsMaterial.key, cert: tlsMaterial.cert },
    async fetch(req) {
      const url = new URL(req.url);
      if (req.method === 'GET' && url.pathname === '/.well-known/openid-configuration') {
        audit.discoveries += 1;
        return Response.json({
          issuer: OIDC_ISSUER,
          authorization_endpoint: `${OIDC_ISSUER}/authorize`,
          token_endpoint: `${OIDC_ISSUER}/token`,
          userinfo_endpoint: `${OIDC_ISSUER}/userinfo`,
          jwks_uri: `${OIDC_ISSUER}/jwks`,
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
        const reqClientId = url.searchParams.get('client_id');
        const redirectUri = url.searchParams.get('redirect_uri');
        const state = url.searchParams.get('state');
        const nonce = url.searchParams.get('nonce');
        const codeChallenge = url.searchParams.get('code_challenge');
        const method = url.searchParams.get('code_challenge_method');
        if (
          reqClientId !== OIDC_CLIENT_ID || !redirectUri || !state || !nonce ||
          method !== 'S256' || !codeChallenge
        ) {
          return new Response('invalid authorize parameters', { status: 400 });
        }
        // Optional negative-test mode triggered by QA client header `x-qa-oidc-error`.
        const forcedError = req.headers.get('x-qa-oidc-error');
        if (forcedError) {
          audit.deniedAuthorizes += 1;
          const target = new URL(redirectUri);
          target.searchParams.set('error', forcedError);
          target.searchParams.set('state', state);
          return Response.redirect(target.toString(), 302);
        }
        audit.authorizes += 1;
        const code = `code-${crypto.randomBytes(16).toString('hex')}`;
        codes.set(code, { redirectUri, nonce, codeChallenge, used: false });
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
        if (form.get('client_id') !== OIDC_CLIENT_ID || form.get('client_secret') !== OIDC_CLIENT_SECRET) {
          return new Response('invalid client credentials', { status: 401 });
        }
        const entry = codes.get(form.get('code') ?? '');
        if (!entry || entry.used || form.get('redirect_uri') !== entry.redirectUri) {
          return new Response('invalid authorization code', { status: 400 });
        }
        const expectedChallenge = b64u(
          crypto.createHash('sha256').update(form.get('code_verifier') ?? '').digest(),
        );
        if (expectedChallenge !== entry.codeChallenge) {
          return new Response('PKCE S256 verification failed', { status: 400 });
        }
        entry.used = true;
        const accessToken = `at-${crypto.randomBytes(16).toString('hex')}`;
        accessTokens.set(accessToken, { sub: OIDC_SUBJECT });
        const iat = Math.floor(Date.now() / 1000);
        const idToken = signJwt({
          iss: OIDC_ISSUER,
          sub: OIDC_SUBJECT,
          aud: OIDC_CLIENT_ID,
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
        const header = req.headers.get('authorization') ?? '';
        const token = header.startsWith('Bearer ') ? header.slice(7) : '';
        const info = accessTokens.get(token);
        if (!info) return new Response('unauthorized', { status: 401 });
        return Response.json({ sub: info.sub, name: 'Pip Deploy QA Owner' });
      }
      return new Response('not found', { status: 404 });
    },
  });

  return { server, audit };
}

// -----------------------------------------------------------------------------
// 4. Mock OpenAI-compatible streaming model
// -----------------------------------------------------------------------------
function startMockModel() {
  const stats = { calls: 0, lastUserTexts: [] };
  const server = Bun.serve({
    hostname: '127.0.0.1',
    port: MODEL_PORT,
    async fetch(req) {
      if (req.method !== 'POST' || !new URL(req.url).pathname.endsWith('/chat/completions')) {
        return new Response('not found', { status: 404 });
      }
      const body = await req.json();
      stats.calls += 1;
      const lastUser = [...(body.messages ?? [])].reverse().find((m) => m.role === 'user');
      const userText = typeof lastUser?.content === 'string' ? lastUser.content : '';
      stats.lastUserTexts.push(userText);
      const reply = `Deployment rehearsal turn ${stats.calls} acknowledged cleanly.`;
      if (!body.stream) {
        return Response.json({
          id: `chatcmpl-qa-${stats.calls}`,
          object: 'chat.completion',
          created: Math.floor(Date.now() / 1000),
          model: 'qa-fixture',
          choices: [{ index: 0, message: { role: 'assistant', content: reply }, finish_reason: 'stop' }],
          usage: { prompt_tokens: 4, completion_tokens: 6, total_tokens: 10 },
        });
      }
      const chunk = (delta, finish) =>
        `data: ${JSON.stringify({
          id: `chatcmpl-qa-${stats.calls}`,
          object: 'chat.completion.chunk',
          created: Math.floor(Date.now() / 1000),
          model: 'qa-fixture',
          choices: [{ index: 0, delta, finish_reason: finish ?? null }],
          ...(finish ? { usage: { prompt_tokens: 4, completion_tokens: 6, total_tokens: 10 } } : {}),
        })}\n\n`;
      const payload =
        chunk({ role: 'assistant', content: reply }, null) +
        chunk({}, 'stop') +
        'data: [DONE]\n\n';
      return new Response(payload, {
        headers: { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' },
      });
    },
  });
  return { server, stats };
}

// -----------------------------------------------------------------------------
// 5. TLS-terminating TCP reverse proxy (supports HTTP/1.1 + WebSocket upgrades)
// -----------------------------------------------------------------------------
function startTlsProxy({ listenPort, upstreamPort, key, cert, name }) {
  const stats = { connections: 0 };
  const server = tls.createServer({ key, cert, ALPNProtocols: ['http/1.1'] }, (clientSock) => {
    stats.connections += 1;
    const upstream = net.connect({ host: '127.0.0.1', port: upstreamPort });
    clientSock.pipe(upstream);
    upstream.pipe(clientSock);
    const cleanup = () => {
      clientSock.destroy();
      upstream.destroy();
    };
    clientSock.on('error', cleanup);
    upstream.on('error', cleanup);
    clientSock.on('close', () => upstream.end());
    upstream.on('close', () => clientSock.end());
  });
  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(listenPort, '0.0.0.0', () => {
      console.error(`[qa-backend] TLS proxy ${name} listening on 0.0.0.0:${listenPort} -> 127.0.0.1:${upstreamPort}`);
      resolve({ server, stats });
    });
  });
}

// -----------------------------------------------------------------------------
// Boot sequence
// -----------------------------------------------------------------------------
const tlsMaterial = generateTls(SECRETS_DIR);
const { secretsEnv, deviceId } = provisionIdentity();
const oidc = startMockOidc(tlsMaterial);
const model = startMockModel();
const backendProxy = await startTlsProxy({
  listenPort: BACKEND_TLS_PORT,
  upstreamPort: 8080,
  key: tlsMaterial.key,
  cert: tlsMaterial.cert,
  name: 'registration',
});
const rivetProxy = await startTlsProxy({
  listenPort: RIVET_TLS_PORT,
  upstreamPort: 6420,
  key: tlsMaterial.key,
  cert: tlsMaterial.cert,
  name: 'rivet',
});

const serverLogs = [];
const serverProc = spawn('/usr/local/bin/robot-buddy-server', [], {
  env: {
    ...process.env,
    ...secretsEnv,
    BUDDY_ENGINE_BINARY: '/usr/local/bin/rivet-engine',
    BUDDY_DATA_DIR: DATA_DIR,
    RIVET_ENDPOINT: 'http://127.0.0.1:6420',
    RIVETKIT_ENGINE_SPAWN: 'never',
    RIVET_NAMESPACE: 'default',
    RIVET_POOL: 'default',
    BUDDY_PUBLIC_URL: PUBLIC_URL,
    BUDDY_PUBLIC_RIVET_URL: PUBLIC_RIVET_URL,
    OIDC_ISSUER,
    OIDC_CLIENT_ID,
    OIDC_CLIENT_SECRET,
    AI_PROVIDER: 'openai_compatible',
    AI_BASE_URL: `http://127.0.0.1:${MODEL_PORT}/v1`,
    AI_API_KEY: 'qa-fixture-model-key-not-real',
    AI_MODEL: 'qa-fixture',
    SSL_CERT_FILE: tlsMaterial.bundlePath,
    RUST_LOG: 'info',
  },
  stdio: ['ignore', 'pipe', 'pipe'],
});

const recordLog = (stream, chunk) => {
  const text = chunk.toString('utf8');
  for (const line of text.split('\n')) {
    if (!line.trim()) continue;
    serverLogs.push(`[${stream}] ${line}`);
    if (serverLogs.length > 500) serverLogs.shift();
  }
  process.stderr.write(text);
};
serverProc.stdout.on('data', (d) => recordLog('out', d));
serverProc.stderr.on('data', (d) => recordLog('err', d));

// Wait until http://127.0.0.1:8080/ready returns 200.
let ready = false;
const deadline = Date.now() + 60_000;
while (Date.now() < deadline) {
  if (serverProc.exitCode !== null) {
    throw new Error(`robot-buddy-server exited prematurely with code ${serverProc.exitCode}`);
  }
  try {
    const res = await fetch('http://127.0.0.1:8080/ready');
    if (res.status === 200) {
      ready = true;
      break;
    }
  } catch {
    // Still starting
  }
  await sleep(300);
}
if (!ready) {
  throw new Error('robot-buddy-server did not become ready within 60 s');
}

const controlServer = Bun.serve({
  hostname: '0.0.0.0',
  port: CONTROL_PORT,
  async fetch(req) {
    const url = new URL(req.url);
    if (url.pathname === '/status') {
      let readyStatus = 0;
      try {
        readyStatus = (await fetch('http://127.0.0.1:8080/ready')).status;
      } catch {}
      return Response.json({
        ready: readyStatus === 200,
        readyStatus,
        deviceId,
        publicUrl: PUBLIC_URL,
        publicRivetUrl: PUBLIC_RIVET_URL,
        oidc: oidc.audit,
        model: { calls: model.stats.calls, turnsRecorded: model.stats.lastUserTexts.length },
        proxies: {
          registrationConnections: backendProxy.stats.connections,
          rivetConnections: rivetProxy.stats.connections,
        },
        serverAlive: serverProc.exitCode === null,
      });
    }
    if (url.pathname === '/logs') {
      return Response.json({ lines: serverLogs.slice(-150) });
    }
    return new Response('not found', { status: 404 });
  },
});

console.error(`[qa-backend] ready! deviceId=${deviceId} public=${PUBLIC_URL} rivet=${PUBLIC_RIVET_URL} oidc=${OIDC_ISSUER}`);

const shutdown = () => {
  controlServer.stop(true);
  oidc.server.stop(true);
  model.server.stop(true);
  backendProxy.server.close();
  rivetProxy.server.close();
  serverProc.kill('SIGTERM');
  setTimeout(() => process.exit(0), 1500);
};
process.on('SIGTERM', shutdown);
process.on('SIGINT', shutdown);
