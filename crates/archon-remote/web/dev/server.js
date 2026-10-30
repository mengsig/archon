// Dev server: serves the shipped web files under the production CSP, stubs
// the pairing API, and bridges /ws to a real archond over its Unix socket.
//   ARCHON_SOCKET=/path/to/archond.sock node dev/server.js [--port 8080]
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { randomBytes } from 'node:crypto';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { WebSocketServer } from 'ws';
import { isShipped } from './shipped.js';
import { bridgeSocket } from './bridge.js';
import { detectProtocolVersion, socketRequest } from '../tests/support/archond.js';
import { normalizePairingCode } from '../src/core/pairing-code.js';

export const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const DEV_CODE = 'DEVCODE1';
export const CSP = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; "
  + "manifest-src 'self'; worker-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const COOKIE = 'archon_dev_session';
const MAX_BODY_BYTES = 16 * 1024;
const MIME = {
  '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml', '.png': 'image/png', '.webmanifest': 'application/manifest+json', '.json': 'application/json',
};

// requirePairing: false answers every session as the paired "Dev browser";
// true makes a browser claim DEVCODE1 first, as the real host does.
export async function startDevServer({ socket, port = 0, host = '127.0.0.1', root = WEB_ROOT, requirePairing = false } = {}) {
  assert.ok(typeof socket === 'string' && socket.length > 0, 'startDevServer needs the archond socket (ARCHON_SOCKET)');
  const protocolVersion = await detectProtocolVersion(socket);
  const status = await socketRequest(socket, protocolVersion, { cmd: 'status' });
  assert.equal(status.reply, 'status', 'archond answered status with something else');
  const state = {
    socket, root, requirePairing, protocolVersion,
    daemonVersion: status.data.daemon_version,
    devices: new Map(),
    defaultDevice: { id: 'dev_browser', name: 'Dev browser', paired_at: new Date().toISOString() },
  };
  const server = createServer((req, res) => {
    route(state, req, res).catch((err) => {
      console.error(`dev server: ${req.method} ${req.url}: ${err.stack ?? err}`);
      if (!res.headersSent) sendJson(res, 500, { error: { code: 'internal', message: err.message } });
      else res.destroy(err);
    });
  });
  const wss = new WebSocketServer({ noServer: true, handleProtocols: (protocols) => (protocols.has('archon') ? 'archon' : false) });
  server.on('upgrade', (req, sock, head) => upgrade(state, wss, req, sock, head));
  await new Promise((done, fail) => server.once('error', fail).listen(port, host, done));
  const address = server.address();
  return {
    url: `http://${host}:${address.port}`,
    port: address.port,
    protocolVersion,
    // Closes every live socket with a code, e.g. 4401 to act out a revocation.
    closeClients: (code, reason = '') => wss.clients.forEach((ws) => ws.close(code, reason)),
    close: () => new Promise((done) => {
      wss.clients.forEach((ws) => ws.terminate());
      server.close(done);
      server.closeAllConnections();
    }),
  };
}

async function route(state, req, res) {
  const url = new URL(req.url, 'http://dev.invalid');
  if (url.pathname.startsWith('/api/')) return api(state, req, res, url.pathname);
  if (req.method !== 'GET' && req.method !== 'HEAD') return sendJson(res, 405, { error: { code: 'bad_request', message: 'method not allowed' } });
  return serveFile(state.root, url.pathname, req, res);
}

async function api(state, req, res, path) {
  const device = sessionDevice(state, req);
  if (req.method === 'GET' && path === '/api/health') {
    return sendJson(res, 200, { status: 'ok', pairing_open: true, version: state.daemonVersion });
  }
  if (req.method === 'GET' && path === '/api/session') {
    if (!device) return sendJson(res, 401, { error: { code: 'unauthorized', message: 'this device is not paired' } });
    return sendJson(res, 200, { device, protocol_version: state.protocolVersion, daemon_version: state.daemonVersion });
  }
  if (req.method === 'POST' && path === '/api/pair/claim') return claim(state, req, res);
  if (req.method === 'POST' && path === '/api/session/forget') {
    if (!device) return sendJson(res, 401, { error: { code: 'unauthorized', message: 'this device is not paired' } });
    state.devices.delete(cookieToken(req));
    res.setHeader('Set-Cookie', `${COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0`);
    return sendJson(res, 200, {});
  }
  return sendJson(res, 404, { error: { code: 'not_found', message: `no API at ${req.method} ${path}` } });
}

async function claim(state, req, res) {
  const body = await readJson(req);
  if (!body || typeof body.code !== 'string' || typeof body.device_name !== 'string') {
    return sendJson(res, 400, { error: { code: 'bad_request', message: 'send {"code","device_name"} as JSON' } });
  }
  const name = body.device_name.trim();
  if (name.length < 1 || name.length > 64) {
    return sendJson(res, 400, { error: { code: 'bad_request', message: 'a device name has 1 to 64 characters' } });
  }
  if (normalizePairingCode(body.code) !== normalizePairingCode(DEV_CODE)) {
    return sendJson(res, 403, { error: { code: 'invalid_code', message: 'that pairing code is not valid' } });
  }
  const token = randomBytes(24).toString('hex');
  const device = { id: `dev_${token.slice(0, 12)}`, name, paired_at: new Date().toISOString() };
  state.devices.set(token, device);
  res.setHeader('Set-Cookie', `${COOKIE}=${token}; Path=/; HttpOnly; SameSite=Strict`);
  return sendJson(res, 200, { device });
}

function sessionDevice(state, req) {
  const paired = state.devices.get(cookieToken(req));
  if (paired) return paired;
  return state.requirePairing ? null : state.defaultDevice;
}

function cookieToken(req) {
  const header = req.headers.cookie ?? '';
  const match = header.split(';').map((part) => part.trim()).find((part) => part.startsWith(`${COOKIE}=`));
  return match ? match.slice(COOKIE.length + 1) : '';
}

function upgrade(state, wss, req, sock, head) {
  const url = new URL(req.url, 'http://dev.invalid');
  const origin = req.headers.origin ?? '';
  if (url.pathname !== '/ws' || origin !== `http://${req.headers.host}`) {
    sock.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
    return;
  }
  wss.handleUpgrade(req, sock, head, (ws) => {
    if (!sessionDevice(state, req)) {
      ws.close(4401, 'this device is not paired');
      return;
    }
    bridgeSocket(ws, state.socket);
  });
}

async function serveFile(root, pathname, req, res) {
  let rel;
  try {
    rel = decodeURIComponent(pathname).replace(/^\/+/, '');
  } catch {
    return sendText(res, 400, 'bad path');
  }
  if (rel === '' || rel === 'pair') rel = 'index.html';
  const full = resolve(root, rel);
  if (!full.startsWith(`${root}/`) || !isShipped(rel)) return sendText(res, 404, 'not found');
  let body;
  try {
    body = await readFile(join(root, rel));
  } catch (err) {
    if (err.code === 'ENOENT' || err.code === 'EISDIR') return sendText(res, 404, 'not found');
    throw err;
  }
  securityHeaders(res);
  res.writeHead(200, { 'Content-Type': MIME[extname(rel)] ?? 'application/octet-stream', 'Cache-Control': 'no-cache' });
  res.end(req.method === 'HEAD' ? undefined : body);
}

function securityHeaders(res) {
  res.setHeader('Content-Security-Policy', CSP);
  res.setHeader('X-Content-Type-Options', 'nosniff');
  res.setHeader('Referrer-Policy', 'no-referrer');
  res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
}

function readJson(req) {
  return new Promise((done, fail) => {
    const chunks = [];
    let size = 0;
    req.on('data', (chunk) => {
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        req.destroy();
        fail(new Error('request body too large'));
        return;
      }
      chunks.push(chunk);
    });
    req.on('end', () => {
      try {
        done(JSON.parse(Buffer.concat(chunks).toString('utf8')));
      } catch {
        done(null);
      }
    });
    req.on('error', fail);
  });
}

function sendJson(res, status, value) {
  securityHeaders(res);
  res.writeHead(status, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
  res.end(JSON.stringify(value));
}

function sendText(res, status, text) {
  securityHeaders(res);
  res.writeHead(status, { 'Content-Type': 'text/plain; charset=utf-8' });
  res.end(text);
}

async function main() {
  const socket = process.env.ARCHON_SOCKET;
  if (!socket) {
    console.error('Set ARCHON_SOCKET to the archond socket, e.g. ARCHON_SOCKET=$XDG_RUNTIME_DIR/archon/archond.sock');
    process.exit(2);
  }
  const at = process.argv.indexOf('--port');
  const port = at > 0 ? Number(process.argv[at + 1]) : 8080;
  assert.ok(Number.isInteger(port) && port >= 0 && port < 65536, `--port must be a port number, got ${process.argv[at + 1]}`);
  const dev = await startDevServer({ socket, port, requirePairing: process.argv.includes('--require-pairing') });
  console.log(`Archon web app on ${dev.url} (archond protocol ${dev.protocolVersion}); pairing code ${DEV_CODE}`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((err) => {
    console.error(`dev server failed: ${err.stack ?? err}`);
    process.exit(1);
  });
}
