// The daemon protocol over one WebSocket: hello first, replies matched to
// requests by req id, feed pushes forwarded, reconnects with backoff.
import { backoffDelay, DISABLED_RETRY_MS } from './backoff.js';

export const CLOSE_REVOKED = 4401;
export const CLOSE_DISABLED = 4403;
const CLOSE_HELLO_FAILED = 4000;
const CLOSE_RESUBSCRIBE = 4001;
export const REQUEST_TIMEOUT_MS = 20_000;

export class DaemonError extends Error {
  constructor(code, message) {
    super(message || code);
    this.name = 'DaemonError';
    this.code = code;
  }
}

// `hello` holds the hello frame's fields; the transport adds `req` and `cmd`.
export function createTransport(options) {
  return new Transport(options);
}

class Transport {
  constructor({
    url, hello, protocol = 'archon', WebSocketImpl = globalThis.WebSocket,
    setTimer = globalThis.setTimeout.bind(globalThis), clearTimer = globalThis.clearTimeout.bind(globalThis),
    random = Math.random, requestTimeoutMs = REQUEST_TIMEOUT_MS,
    onFeed = () => {}, onState = () => {}, onReady = () => {}, onProtocolError = () => {},
  }) {
    if (typeof url !== 'string' || !/^wss?:\/\//.test(url)) throw new TypeError(`a WebSocket URL, got ${url}`);
    if (!hello || !Number.isInteger(hello.protocol_version)) throw new TypeError('hello needs an integer protocol_version');
    if (typeof WebSocketImpl !== 'function') throw new TypeError('no WebSocket implementation');
    Object.assign(this, { url, hello, protocol, WebSocketImpl, setTimer, clearTimer, random, requestTimeoutMs });
    this.on = { feed: onFeed, state: onState, ready: onReady, protocolError: onProtocolError };
    this.ws = null;
    this.nextReq = 1;
    this.attempt = 0;
    this.retryTimer = null;
    this.current = 'idle';
    this.stopped = false;
    this.closeReason = null;
    this.restarting = false;
    this.pending = new Map();
    this.queued = [];
  }

  get state() {
    return this.current;
  }

  start() {
    if (this.current !== 'idle') throw new Error(`the transport already started (${this.current})`);
    this.connect();
  }

  stop() {
    this.stopped = true;
    if (this.retryTimer !== null) this.clearTimer(this.retryTimer);
    this.rejectQueued(new DaemonError('disconnected', 'the app stopped'));
    if (this.ws) this.ws.close(1000, 'bye');
    this.setState('stopped');
  }

  // Skips the rest of a backoff wait, e.g. when the page becomes visible again.
  reconnectNow() {
    if (this.stopped || this.retryTimer === null) return;
    this.clearTimer(this.retryTimer);
    this.connect();
  }

  request(cmd, fields = {}) {
    if (typeof cmd !== 'string' || cmd === 'hello') throw new TypeError(`not a request command: ${cmd}`);
    if (this.current === 'revoked' || this.stopped) return Promise.reject(new DaemonError('unauthorized', 'not connected'));
    return new Promise((resolve, reject) => {
      const entry = { cmd, fields, resolve, reject, timer: null };
      entry.timer = this.setTimer(() => this.expire(entry), this.requestTimeoutMs);
      if (this.current === 'open') this.dispatch(entry);
      else this.queued.push(entry);
    });
  }

  setState(next, info = {}) {
    this.current = next;
    this.on.state(next, info);
  }

  connect() {
    if (this.stopped) return;
    this.retryTimer = null;
    this.nextReq = 1;
    this.closeReason = null;
    this.setState('connecting', { attempt: this.attempt });
    const socket = new this.WebSocketImpl(this.url, this.protocol);
    this.ws = socket;
    socket.onopen = () => socket === this.ws && this.sendHello(socket);
    socket.onmessage = (event) => socket === this.ws && this.receive(event.data);
    socket.onclose = (event) => socket === this.ws && this.closed(event.code, event.reason);
    // Errors always end in a close event, which carries the handling.
    socket.onerror = () => {};
  }

  sendHello(socket) {
    const req = this.nextReq++;
    const fail = (message) => {
      this.closeReason = message;
      socket.close(CLOSE_HELLO_FAILED, 'hello failed');
    };
    this.pending.set(req, {
      timer: this.setTimer(() => fail('the daemon did not answer hello'), this.requestTimeoutMs),
      resolve: (welcome) => this.opened(welcome),
      reject: (err) => fail(err.message),
    });
    socket.send(JSON.stringify({ req, cmd: 'hello', ...this.hello }));
  }

  opened(welcome) {
    // A daemon that keeps ending the feed backs off as if it kept dropping.
    if (!this.restarting) this.attempt = 0;
    this.restarting = false;
    this.setState('open', { welcome });
    for (const entry of this.queued.splice(0)) this.dispatch(entry);
    this.on.ready(welcome);
  }

  receive(text) {
    let frame;
    try {
      frame = JSON.parse(text);
    } catch (err) {
      this.on.protocolError(new Error(`the daemon sent a frame that is not JSON: ${String(text).slice(0, 80)}`, { cause: err }));
      return;
    }
    if (frame.frame === 'feed') {
      this.on.feed(frame.item, frame.sub);
      return;
    }
    if (frame.frame !== 'reply' || !Number.isInteger(frame.id)) {
      this.on.protocolError(new Error(`unexpected frame: ${String(text).slice(0, 80)}`));
      return;
    }
    if (frame.id === 0) {
      this.unaddressed(frame.result?.Err);
      return;
    }
    this.settle(frame.id, frame.result);
  }

  // An id-0 error explains a close the daemon is about to make, except an
  // internal one: a feed subscription ended, so reconnect and resubscribe.
  unaddressed(err) {
    this.closeReason = err?.message ?? 'the daemon closed the connection';
    if (err?.code !== 'internal' || this.current !== 'open' || !this.ws) return;
    this.on.protocolError(new DaemonError('internal', `${this.closeReason}; reconnecting`));
    this.restarting = true;
    this.ws.close(CLOSE_RESUBSCRIBE, 'resubscribe');
  }

  settle(id, result) {
    const entry = this.pending.get(id);
    if (!entry) return;
    this.pending.delete(id);
    if (entry.timer !== null) this.clearTimer(entry.timer);
    if (result && 'Ok' in result) entry.resolve(result.Ok);
    else entry.reject(new DaemonError(result?.Err?.code ?? 'internal', result?.Err?.message ?? 'malformed reply'));
  }

  closed(code, reason) {
    this.ws = null;
    const why = this.closeReason || reason || '';
    this.failPending(new DaemonError('disconnected', why || 'connection lost'));
    if (this.stopped) return;
    const info = { closeCode: code, reason: why };
    if (code === CLOSE_REVOKED) {
      this.rejectQueued(new DaemonError('unauthorized', 'this device is no longer paired'));
      this.setState('revoked', info);
      return;
    }
    const delay = code === CLOSE_DISABLED ? DISABLED_RETRY_MS : backoffDelay(this.attempt, this.random);
    this.attempt += 1;
    this.retryTimer = this.setTimer(() => this.connect(), delay);
    this.setState(code === CLOSE_DISABLED ? 'disabled' : 'waiting', { ...info, retryInMs: delay, attempt: this.attempt });
  }

  failPending(err) {
    for (const entry of this.pending.values()) {
      if (entry.timer !== null) this.clearTimer(entry.timer);
      entry.reject(err);
    }
    this.pending.clear();
  }

  rejectQueued(err) {
    for (const entry of this.queued.splice(0)) {
      this.clearTimer(entry.timer);
      entry.reject(err);
    }
  }

  dispatch(entry) {
    if (!this.ws || this.current !== 'open') throw new Error('dispatch needs an open connection');
    const req = this.nextReq++;
    this.pending.set(req, entry);
    this.ws.send(JSON.stringify({ ...entry.fields, req, cmd: entry.cmd }));
  }

  expire(entry) {
    const at = this.queued.indexOf(entry);
    if (at >= 0) this.queued.splice(at, 1);
    for (const [id, value] of this.pending) if (value === entry) this.pending.delete(id);
    entry.reject(new DaemonError('timeout', `${entry.cmd} got no answer in ${Math.round(this.requestTimeoutMs / 1000)} s`));
  }
}
