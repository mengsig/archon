import test from 'node:test';
import assert from 'node:assert/strict';
import { createTransport, DaemonError, CLOSE_REVOKED, CLOSE_DISABLED } from '../../src/core/transport.js';
import { backoffDelay, DISABLED_RETRY_MS } from '../../src/core/backoff.js';

// A scripted WebSocket and clock: tests open, answer and close sockets by hand.
function harness(extra = {}) {
  const sockets = [];
  const timers = [];
  const states = [];
  const feed = [];
  class FakeSocket {
    constructor(url, protocol) {
      Object.assign(this, { url, protocol, sent: [], closedWith: null });
      sockets.push(this);
    }
    send(text) { this.sent.push(JSON.parse(text)); }
    close(code) { this.closedWith = code; this.onclose?.({ code, reason: '' }); }
    open() { this.onopen?.(); }
    reply(id, result) { this.onmessage?.({ data: JSON.stringify({ frame: 'reply', id, result }) }); }
    welcome() { this.reply(this.sent[0].req, { Ok: { reply: 'welcome', data: { protocol_version: 23 } } }); }
    drop(code = 1006) { this.onclose?.({ code, reason: '' }); }
  }
  const transport = createTransport({
    url: 'ws://host/ws',
    hello: { protocol_version: 23, client: 'archon-web', client_version: 'web' },
    WebSocketImpl: FakeSocket,
    setTimer: (fn, ms) => { const t = { fn, ms, done: false }; timers.push(t); return t; },
    clearTimer: (t) => { if (t) t.done = true; },
    random: () => 0.5,
    onState: (state, info) => states.push({ state, ...info }),
    onFeed: (item) => feed.push(item),
    ...extra,
  });
  const fire = (ms) => {
    const due = timers.filter((t) => !t.done && t.ms === ms);
    assert.ok(due.length > 0, `a timer of ${ms} ms is pending`);
    for (const t of due) { t.done = true; t.fn(); }
  };
  return { transport, sockets, timers, states, feed, fire, last: () => sockets.at(-1) };
}

test('hello is the first frame, as req 1, and opens the transport', () => {
  const h = harness();
  h.transport.start();
  assert.equal(h.last().protocol, 'archon');
  h.last().open();
  assert.deepEqual(h.last().sent[0], { req: 1, cmd: 'hello', protocol_version: 23, client: 'archon-web', client_version: 'web' });
  h.last().welcome();
  assert.equal(h.transport.state, 'open');
});

test('replies settle the request with the same req id, in any order', async () => {
  const h = harness();
  h.transport.start(); h.last().open(); h.last().welcome();
  const status = h.transport.request('status');
  const items = h.transport.request('list_items', { states: [] });
  const [a, b] = h.last().sent.slice(1);
  assert.deepEqual([a.req, a.cmd, b.req, b.cmd], [2, 'status', 3, 'list_items']);
  h.last().reply(3, { Ok: { reply: 'items', data: [] } });
  h.last().reply(2, { Err: { code: 'forbidden', message: 'no' } });
  assert.deepEqual(await items, { reply: 'items', data: [] });
  await assert.rejects(status, (err) => err instanceof DaemonError && err.code === 'forbidden');
});

test('requests made before the welcome wait for it', async () => {
  const h = harness();
  h.transport.start();
  const pending = h.transport.request('status');
  h.last().open();
  assert.equal(h.last().sent.length, 1, 'only hello goes before the welcome');
  h.last().welcome();
  assert.equal(h.last().sent[1].cmd, 'status');
  h.last().reply(2, { Ok: { reply: 'ok' } });
  assert.deepEqual(await pending, { reply: 'ok' });
});

test('an unanswered request times out', async () => {
  const h = harness({ requestTimeoutMs: 1234 });
  h.transport.start(); h.last().open(); h.last().welcome();
  const pending = h.transport.request('status');
  h.fire(1234);
  await assert.rejects(pending, (err) => err.code === 'timeout');
  h.last().reply(2, { Ok: { reply: 'ok' } });
});

test('a dropped connection fails in-flight requests and reconnects with backoff', async () => {
  const h = harness();
  h.transport.start(); h.last().open(); h.last().welcome();
  const inflight = h.transport.request('status');
  h.last().drop();
  await assert.rejects(inflight, (err) => err.code === 'disconnected');
  const first = backoffDelay(0, () => 0.5);
  assert.equal(h.states.at(-1).state, 'waiting');
  assert.equal(h.states.at(-1).retryInMs, first);
  h.fire(first);
  assert.equal(h.sockets.length, 2);
  h.last().drop();
  assert.equal(h.states.at(-1).retryInMs, backoffDelay(1, () => 0.5));
  h.fire(backoffDelay(1, () => 0.5));
  h.last().open();
  assert.equal(h.last().sent[0].req, 1, 'each connection starts again at req 1');
  h.last().welcome();
  h.last().drop();
  assert.equal(h.states.at(-1).retryInMs, first, 'a welcome resets the backoff');
});

test('4401 stops for good and 4403 retries slowly', async () => {
  const revoked = harness();
  revoked.transport.start(); revoked.last().open(); revoked.last().welcome();
  revoked.last().drop(CLOSE_REVOKED);
  assert.equal(revoked.transport.state, 'revoked');
  assert.equal(revoked.timers.filter((t) => !t.done).length, 0, 'no reconnect is scheduled');
  await assert.rejects(revoked.transport.request('status'), (err) => err.code === 'unauthorized');

  const disabled = harness();
  disabled.transport.start(); disabled.last().drop(CLOSE_DISABLED);
  assert.equal(disabled.transport.state, 'disabled');
  disabled.fire(DISABLED_RETRY_MS);
  assert.equal(disabled.sockets.length, 2);
});

test('feed pushes are forwarded and id 0 explains the close', () => {
  const h = harness();
  h.transport.start(); h.last().open(); h.last().welcome();
  h.last().onmessage({ data: JSON.stringify({ frame: 'feed', sub: 1, item: { seq: 4 } }) });
  assert.deepEqual(h.feed, [{ seq: 4 }]);
  h.last().reply(0, { Err: { code: 'unavailable', message: 'archond is stopping' } });
  h.last().drop(1000);
  assert.equal(h.states.at(-1).reason, 'archond is stopping');
});

test('an internal id-0 error reconnects to resubscribe, backing off while it repeats', () => {
  const errors = [];
  const h = harness({ onProtocolError: (err) => errors.push(err) });
  h.transport.start(); h.last().open(); h.last().welcome();
  h.last().reply(0, { Err: { code: 'internal', message: 'feed subscription 1 ended: disk full' } });
  assert.equal(h.sockets[0].closedWith, 4001);
  assert.match(errors[0].message, /feed subscription 1 ended: disk full; reconnecting/);
  assert.equal(h.states.at(-1).state, 'waiting');
  h.fire(backoffDelay(0, () => 0.5));
  h.last().open(); h.last().welcome();
  h.last().reply(0, { Err: { code: 'internal', message: 'feed subscription 1 ended: disk full' } });
  assert.equal(h.states.at(-1).retryInMs, backoffDelay(1, () => 0.5), 'the backoff was not reset');
  assert.equal(errors.length, 2);
});

test('a hello without an answer closes the socket and retries', () => {
  const h = harness();
  h.transport.start(); h.last().open();
  h.fire(20_000);
  assert.equal(h.last().closedWith, 4000);
  const waiting = h.states.at(-1);
  assert.equal(waiting.state, 'waiting');
  assert.match(waiting.reason, /did not answer hello/);
});

test('backoff grows, is capped and keeps half its delay fixed', () => {
  assert.equal(backoffDelay(0, () => 0), 250);
  assert.equal(backoffDelay(0, () => 0.998), 500);
  assert.equal(backoffDelay(3, () => 0), 2000);
  assert.equal(backoffDelay(50, () => 0), 15_000);
  assert.ok(backoffDelay(50, () => 0.9999) <= 30_000, 'never past the cap');
  assert.throws(() => backoffDelay(-1), RangeError);
  assert.throws(() => backoffDelay(0, () => 1), RangeError);
});
