import test from 'node:test';
import assert from 'node:assert/strict';
import { createTransport, DaemonError, CLOSE_REVOKED, CLOSE_DISABLED, MAX_IN_FLIGHT, LATE_REPLY_MS } from '../../src/core/transport.js';
import { backoffDelay, retryDelay, DISABLED_RETRY_MS } from '../../src/core/backoff.js';

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
  const slow = disabled.states.at(-1).retryInMs;
  assert.equal(slow, retryDelay(0, { minMs: DISABLED_RETRY_MS, random: () => 0.5 }));
  assert.ok(slow > DISABLED_RETRY_MS, 'a disabled listener is retried past its minute, jittered');
  disabled.fire(slow);
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

test('healthy legacy preview visits release the stream without accumulating fault backoff', () => {
  const h = harness();
  const first = backoffDelay(0, () => 0.5);
  h.transport.start();
  for (let visit = 0; visit < 8; visit += 1) {
    h.last().open();
    h.last().welcome();
    h.transport.resubscribe();
    assert.equal(h.last().closedWith, 4001);
    assert.equal(h.states.at(-1).retryInMs, first);
    h.fire(first);
  }
});

test('a change subscription ended by a lag reconnects to resubscribe', () => {
  const errors = [];
  const h = harness({ onProtocolError: (err) => errors.push(err) });
  h.transport.start(); h.last().open(); h.last().welcome();
  const lag = 'change subscription 2 ended: it fell 300 changes behind; resubscribe with after: 41';
  h.last().reply(0, { Err: { code: 'unavailable', message: lag } });
  assert.equal(h.sockets[0].closedWith, 4001, 'the socket is closed to resubscribe');
  assert.equal(errors[0].code, 'unavailable');
  assert.match(errors[0].message, /fell 300 changes behind.*; reconnecting$/);
  assert.equal(h.states.at(-1).state, 'waiting');
  h.fire(backoffDelay(0, () => 0.5));
  h.last().open(); h.last().welcome();
  assert.equal(h.states.at(-1).state, 'open');
  h.last().reply(0, { Err: { code: 'unavailable', message: 'archond is stopping' } });
  assert.equal(h.sockets[1].closedWith, null, 'a stop is explained, not acted on');
  assert.equal(errors.length, 1);
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

test('a request may ask for a longer answer deadline than the default', () => {
  const h = harness();
  h.transport.start(); h.last().open(); h.last().welcome();
  h.transport.request('conversation', { wait: true }, { timeoutMs: 30_000 }).catch(() => {});
  assert.ok(h.timers.some((t) => !t.done && t.ms === 30_000));
  assert.throws(() => h.transport.request('status', {}, { timeoutMs: 0 }), RangeError);
});

test('a page shown again retries soon, jittered, but never cuts a disabled host\'s minute', () => {
  let clock = 0;
  const h = harness({ now: () => clock });
  h.transport.start(); h.last().open(); h.last().welcome();
  h.last().drop(); h.fire(backoffDelay(0, () => 0.5)); h.last().drop();
  h.fire(backoffDelay(1, () => 0.5)); h.last().drop();
  const long = backoffDelay(2, () => 0.5);
  assert.equal(h.states.at(-1).retryInMs, long);
  const soon = retryDelay(0, { random: () => 0.5 });
  assert.ok(soon > 0 && soon < long, 'the wake wait is the shortest jittered one');
  h.transport.reconnectSoon();
  assert.equal(h.states.at(-1).retryInMs, soon, 'the banner names the new wait');
  assert.equal(h.sockets.length, 3, 'no socket opens before the jittered wait');
  h.transport.reconnectSoon();
  assert.equal(h.timers.filter((t) => !t.done && t.ms === soon).length, 1, 'a sooner retry stands');
  h.fire(soon);
  assert.equal(h.sockets.length, 4);

  const disabled = harness({ now: () => clock });
  disabled.transport.start(); disabled.last().drop(CLOSE_DISABLED);
  const minute = disabled.states.at(-1).retryInMs;
  disabled.transport.reconnectSoon();
  disabled.transport.reconnectNow();
  assert.equal(disabled.sockets.length, 1, 'remote access off waits out its minute');
  assert.equal(disabled.timers.filter((t) => !t.done).map((t) => t.ms).join(), String(minute));
});

test('past archond\'s in-flight cap requests wait for a reply instead of being refused', async () => {
  const h = harness();
  h.transport.start(); h.last().open(); h.last().welcome();
  const answers = [];
  for (let i = 0; i < MAX_IN_FLIGHT + 3; i += 1) h.transport.request('design_resolve', { i }).then((r) => answers.push(r));
  h.transport.request('design_frame_ack', { sub: 1, seq: 1 });
  const sent = () => h.last().sent.slice(1);
  assert.equal(sent().filter((f) => f.cmd === 'design_resolve').length, MAX_IN_FLIGHT);
  assert.equal(sent().at(-1).cmd, 'design_frame_ack', 'acks are not counted and never wait');
  assert.equal(h.transport.waiting, 3);
  h.last().reply(sent()[0].req, { Ok: { reply: 'design_done', data: null } });
  const next = sent().at(-1);
  assert.deepEqual([next.cmd, next.i], ['design_resolve', MAX_IN_FLIGHT], 'the oldest waiting request goes next');
  assert.equal(h.transport.waiting, 2);
  await Promise.resolve();
  assert.equal(answers.length, 1);
});

test('a timed-out request keeps its slot until its late reply, and a drop fails waiting requests', async () => {
  const h = harness({ requestTimeoutMs: 1000 });
  h.transport.start(); h.last().open(); h.last().welcome();
  const outcomes = [];
  for (let i = 0; i < MAX_IN_FLIGHT + 1; i += 1) {
    h.transport.request('design_resolve', { i }).then(() => outcomes.push('ok'), (e) => outcomes.push(e.code));
  }
  h.fire(1000);
  await Promise.resolve();
  assert.equal(outcomes.filter((o) => o === 'timeout').length, MAX_IN_FLIGHT + 1);
  assert.equal(h.transport.waiting, 0, 'an expired request no longer waits');
  h.transport.request('design_resolve', { i: 'late' }).catch((e) => outcomes.push(e.code));
  assert.equal(h.transport.waiting, 1, 'archond still holds the expired requests\' slots');
  h.last().reply(h.last().sent[1].req, { Ok: { reply: 'design_resolved', data: null } });
  assert.equal(h.last().sent.at(-1).i, 'late');
  h.transport.request('design_resolve', { i: 'held' }).catch((e) => outcomes.push(e.code));
  h.last().drop();
  await Promise.resolve();
  assert.deepEqual(outcomes.slice(-2), ['disconnected', 'disconnected']);
  assert.equal(h.transport.waiting, 0);
});

test('input past the in-flight cap goes at once, for archond to run or refuse, and never expires unsent', async () => {
  const h = harness({ requestTimeoutMs: 1000 });
  h.transport.start(); h.last().open(); h.last().welcome();
  for (let i = 0; i < MAX_IN_FLIGHT; i += 1) h.transport.request('design_resolve', { i }).catch(() => {});
  h.transport.request('design_resolve', { i: 'waits' }).catch(() => {});
  assert.equal(h.transport.waiting, 1);
  let refused = null;
  h.transport.request('design_input', { events: [] }).catch((e) => { refused = e; });
  const input = h.last().sent.at(-1);
  assert.equal(input.cmd, 'design_input', 'input does not wait behind the cap');
  assert.equal(h.transport.waiting, 1, 'nor does it count as waiting');
  h.last().reply(input.req, { Err: { code: 'unavailable', message: 'at most 16 requests may be in flight' } });
  await Promise.resolve();
  assert.equal(refused?.code, 'unavailable', 'archond\'s refusal reaches the batcher');
  assert.equal(h.last().sent.at(-1).cmd, 'design_input', 'its slot was never archond\'s to give the waiting request');
  h.last().reply(h.last().sent[1].req, { Ok: { reply: 'design_resolved', data: null } });
  assert.equal(h.last().sent.at(-1).i, 'waits', 'an answered request frees the slot for the one waiting');
});

test('slots of requests that never get a reply come free after the grace period', async () => {
  const h = harness({ requestTimeoutMs: 1000 });
  h.transport.start(); h.last().open(); h.last().welcome();
  for (let i = 0; i < MAX_IN_FLIGHT; i += 1) h.transport.request('design_resolve', { i }).catch(() => {});
  h.fire(1000);
  let answer = null;
  h.transport.request('item_get', { id: 'x' }).then((r) => { answer = r; });
  assert.equal(h.transport.waiting, 1, 'hung requests still hold archond\'s slots');
  h.fire(LATE_REPLY_MS);
  const sent = h.last().sent.at(-1);
  assert.equal(sent.cmd, 'item_get', 'the waiting request goes once the slots are presumed lost');
  assert.equal(h.transport.waiting, 0);
  h.last().reply(sent.req, { Ok: { reply: 'item', data: 1 } });
  h.last().reply(h.last().sent[1].req, { Ok: { reply: 'design_done', data: null } });
  await Promise.resolve();
  assert.deepEqual(answer, { reply: 'item', data: 1 });
  assert.equal(h.timers.filter((t) => !t.done && t.ms === LATE_REPLY_MS).length, 0, 'no grace timer is left behind');
});
