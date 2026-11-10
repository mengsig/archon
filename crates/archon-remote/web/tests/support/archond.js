// A private archond for the browser tests and the dev bridge: a temp home,
// the daemon process we started, the CLI against it and raw socket requests.
import assert from 'node:assert/strict';
import { spawn, execFile } from 'node:child_process';
import { createConnection } from 'node:net';
import { mkdtempSync, mkdirSync, writeFileSync, openSync, closeSync, readFileSync, rmSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { promisify } from 'node:util';

const execFileP = promisify(execFile);
const START_WAIT_MS = 60_000;
const REQUEST_TIMEOUT_MS = 15_000;

export function defaultArchonBin() {
  if (process.env.ARCHON_BIN) return process.env.ARCHON_BIN;
  const target = process.env.CARGO_TARGET_DIR;
  assert.ok(target, 'set ARCHON_BIN or CARGO_TARGET_DIR to find the archon binary');
  return join(target, 'debug', 'archon');
}

function archondEnv(home) {
  const env = { ...process.env };
  delete env.AETHER_ENGINE_AGENT;
  delete env.AETHER_ENGINE_RUN;
  return {
    ...env,
    ARCHON_DATA_DIR: join(home, 'data'),
    ARCHON_CONFIG_DIR: join(home, 'config'),
    ARCHON_SOCKET: join(home, 'archond.sock'),
    AETHER_SOCKET: join(home, 'no-aether.sock'),
    AETHER_CONFIG_DIR: join(home, 'aether'),
    ARCHON_NO_UPDATE_CHECK: '1',
    NO_COLOR: '1',
  };
}

// The socket path must stay short (SUN_LEN), so the home lives under /tmp.
function makeHome(config) {
  const home = mkdtempSync('/tmp/arw.');
  mkdirSync(join(home, 'config'), { recursive: true });
  writeFileSync(join(home, 'config', 'config.toml'), `[aether]\nmode = "shared"\n${config}`);
  return home;
}

// `config` is appended to config.toml, after the `[aether]` table.
export async function startArchond({ archonBin = defaultArchonBin(), config = '' } = {}) {
  assert.ok(existsSync(archonBin), `no archon binary at ${archonBin}`);
  assert.equal(typeof config, 'string', 'config is TOML text');
  const home = makeHome(config);
  const env = archondEnv(home);
  const socket = env.ARCHON_SOCKET;
  const logFd = openSync(join(home, 'daemon.log'), 'w');
  const child = spawn(archonBin, ['daemon', 'run'], { env, stdio: ['ignore', logFd, logFd] });
  closeSync(logFd);
  const exited = new Promise((resolve) => child.once('exit', (code, signal) => resolve({ code, signal })));
  const run = (args) => runCli(archonBin, env, args);
  try {
    await waitUntilRunning(run, exited, home);
  } catch (err) {
    child.kill('SIGKILL');
    throw err;
  }
  const protocolVersion = await detectProtocolVersion(socket);
  return {
    home,
    socket,
    env,
    protocolVersion,
    run,
    request: (cmd) => socketRequest(socket, protocolVersion, cmd),
    stop: () => stopArchond({ child, exited, run, home }),
  };
}

async function runCli(archonBin, env, args) {
  assert.ok(Array.isArray(args) && args.length > 0, 'archon needs arguments');
  try {
    const { stdout, stderr } = await execFileP(archonBin, args, { env, maxBuffer: 16 << 20 });
    return { stdout, stderr };
  } catch (err) {
    throw new Error(`archon ${args.join(' ')} failed: ${err.stderr || err.message}`, { cause: err });
  }
}

async function waitUntilRunning(run, exited, home) {
  const deadline = Date.now() + START_WAIT_MS;
  let gone = null;
  exited.then((status) => { gone = status; });
  while (Date.now() < deadline) {
    if (gone) throw new Error(`archond exited early (${JSON.stringify(gone)}):\n${daemonLog(home)}`);
    const up = await run(['daemon', 'status']).then(() => true, () => false);
    if (up) return;
    await delay(150);
  }
  throw new Error(`archond did not start within ${START_WAIT_MS} ms:\n${daemonLog(home)}`);
}

function daemonLog(home) {
  return readFileSync(join(home, 'daemon.log'), 'utf8');
}

async function stopArchond({ child, exited, run, home }) {
  if (child.exitCode === null && child.signalCode === null) {
    await run(['daemon', 'stop']).catch((err) => {
      process.stderr.write(`archon daemon stop failed, killing pid ${child.pid}: ${err.message}\n`);
      child.kill('SIGKILL');
    });
    const timer = setTimeout(() => child.kill('SIGKILL'), 10_000);
    await exited;
    clearTimeout(timer);
  }
  // archond leaves the orchestrator's mirrors read-only; open them before removal.
  const mirrors = join(home, 'data', 'orchestrator', 'repos');
  if (existsSync(mirrors)) await execFileP('chmod', ['-R', 'u+w', mirrors]);
  rmSync(home, { recursive: true, force: true });
}

// The daemon refuses a wrong version and names its own, so ask with 0.
export async function detectProtocolVersion(socket) {
  const conn = await openSocket(socket);
  try {
    conn.send({ req: 1, cmd: 'hello', protocol_version: 0, client: 'archon-cli', client_version: 'probe' });
    const frame = await conn.next((f) => f.frame === 'reply');
    const message = frame.result?.Err?.message ?? '';
    const match = /archond speaks protocol (\d+)/.exec(message);
    assert.ok(match, `unexpected hello refusal: ${JSON.stringify(frame)}`);
    return Number(match[1]);
  } finally {
    conn.close();
  }
}

// One request as the CLI client. Resolves the Ok reply, rejects an Err with its code.
export async function socketRequest(socket, protocolVersion, cmd) {
  assert.ok(cmd && typeof cmd.cmd === 'string', 'a request names its cmd');
  const conn = await openSocket(socket);
  try {
    conn.send({ req: 1, cmd: 'hello', protocol_version: protocolVersion, client: 'archon-cli', client_version: 'web-tests' });
    unwrap(await conn.next((f) => f.frame === 'reply' && (f.id === 1 || f.id === 0)));
    conn.send({ ...cmd, req: 2 });
    return unwrap(await conn.next((f) => f.frame === 'reply' && (f.id === 2 || f.id === 0)));
  } finally {
    conn.close();
  }
}

function unwrap(frame) {
  const { Ok, Err } = frame.result ?? {};
  if (Ok) return Ok;
  const error = new Error(`${Err?.code}: ${Err?.message}`);
  error.code = Err?.code;
  throw error;
}

// An NDJSON connection: send objects, await the next frame that matches.
export function openSocket(path) {
  return new Promise((resolve, reject) => {
    const sock = createConnection(path);
    const frames = [];
    const waiters = [];
    let buffered = '';
    let failure = null;
    const settle = () => {
      for (let i = waiters.length - 1; i >= 0; i -= 1) {
        const waiter = waiters[i];
        const at = frames.findIndex(waiter.match);
        if (at >= 0) {
          waiters.splice(i, 1);
          waiter.resolve(frames.splice(at, 1)[0]);
        } else if (failure) {
          waiters.splice(i, 1);
          waiter.reject(failure);
        }
      }
    };
    sock.setEncoding('utf8');
    sock.on('data', (chunk) => {
      buffered += chunk;
      const lines = buffered.split('\n');
      buffered = lines.pop();
      for (const line of lines) if (line.trim()) frames.push(JSON.parse(line));
      settle();
    });
    sock.on('error', (err) => { failure = err; settle(); reject(err); });
    sock.on('close', () => { failure ??= new Error('archond closed the connection'); settle(); });
    sock.on('connect', () => resolve({
      send: (obj) => sock.write(`${JSON.stringify(obj)}\n`),
      next: (match) => new Promise((res, rej) => {
        const timer = setTimeout(() => rej(new Error('timed out waiting for archond')), REQUEST_TIMEOUT_MS);
        waiters.push({ match, resolve: (f) => { clearTimeout(timer); res(f); }, reject: (e) => { clearTimeout(timer); rej(e); } });
        settle();
      }),
      close: () => sock.end(),
    }));
  });
}

export async function gitRepo(path) {
  mkdirSync(path, { recursive: true });
  await execFileP('git', ['-C', path, 'init', '-q']);
  await execFileP('git', ['-C', path, '-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'init']);
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
