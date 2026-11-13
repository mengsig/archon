import test from 'node:test';
import assert from 'node:assert/strict';
import { createConversation, entryKey, stripRelayMark, WEB_RELAY_MARK, PAGE_LIMIT, POLL_TIMEOUT_MS, MIN_IDLE_POLL_MS } from '../../src/core/conversation.js';

const entry = (seq, part = 0, kind = 'assistant') => ({ seq, part, at: '2026-11-13T10:00:00Z', kind, important: true, text: `e${seq}.${part}` });
const page = (entries, toSeq, extra = {}) => ({ reply: 'conversation', data: { entries, from_seq: 0, to_seq: toSeq, has_before: false, has_after: false, last_seq: toSeq, ...extra } });
const flush = () => new Promise((resolve) => setImmediate(resolve));

// Scripted daemon: each request takes the next queued outcome; an empty queue holds the request open.
function harness(outcomes, extra = {}) {
  const calls = [];
  const timers = [];
  const changes = [];
  const errors = [];
  const queue = [...outcomes];
  let open = true;
  const request = (cmd, fields, options) => {
    calls.push({ cmd, fields, options });
    if (queue.length === 0) return new Promise(() => {});
    const next = queue.shift();
    return next instanceof Error ? Promise.reject(next) : Promise.resolve(next);
  };
  const conversation = createConversation({
    request, isOpen: () => open,
    onChange: (entries, fresh) => changes.push({ entries, fresh }),
    onError: (err) => errors.push(err),
    setTimer: (fn, ms) => { const t = { fn, ms, done: false }; timers.push(t); return t; },
    clearTimer: (t) => { if (t) t.done = true; },
    now: () => 0,
    ...extra,
  });
  const fire = async (ms) => {
    const due = timers.filter((t) => !t.done && t.ms === ms);
    assert.ok(due.length > 0, `a timer of ${ms} ms is pending`);
    for (const t of due) { t.done = true; t.fn(); }
    await flush();
  };
  return { conversation, calls, timers, changes, errors, fire, close: () => { open = false; }, pending: () => timers.filter((t) => !t.done) };
}

test('the newest page loads first, then a long poll continues from to_seq', async () => {
  const h = harness([page([entry(7), entry(8, 0, 'operator'), entry(9)], 9), page([entry(10)], 10)]);
  h.conversation.run();
  await flush();
  assert.deepEqual(h.calls[0], { cmd: 'conversation', fields: { after: null, before: null, limit: PAGE_LIMIT, wait: false }, options: undefined });
  assert.deepEqual(h.calls[1], { cmd: 'conversation', fields: { after: 9, before: null, limit: null, wait: true }, options: { timeoutMs: POLL_TIMEOUT_MS } });
  assert.deepEqual(h.conversation.entries.map(entryKey), ['7-0', '8-0', '9-0', '10-0']);
  assert.equal(h.conversation.cursor, 10);
  assert.equal(h.calls.length, 3, 'the loop polls again from the new cursor');
  assert.equal(h.calls[2].fields.after, 10);
  assert.deepEqual(h.changes.at(-1).fresh.map(entryKey), ['10-0']);
});

test('an empty page still advances the cursor', async () => {
  const h = harness([page([entry(3)], 3), page([], 12)]);
  h.conversation.run();
  await flush();
  assert.equal(h.conversation.cursor, 12);
  assert.equal(h.calls[2].fields.after, 12);
  assert.equal(h.changes.length, 1, 'no change is announced for an empty page');
  assert.equal(h.pending().length, 0, 'progress needs no idle pause');
});

test('the cursor never moves back and repeated entries are dropped', async () => {
  const h = harness([page([entry(5), entry(6)], 6), page([entry(6), entry(6, 1), entry(6, 1), entry(4)], 5)]);
  h.conversation.run();
  await flush();
  assert.deepEqual(h.conversation.entries.map(entryKey), ['5-0', '6-0', '6-1']);
  assert.equal(h.conversation.cursor, 6);
  assert.deepEqual(h.changes.at(-1).fresh.map(entryKey), ['6-1']);
});

test('only the newest entries are kept', async () => {
  const many = Array.from({ length: 5 }, (_, i) => entry(i + 1));
  const h = harness([page(many, 5)], { keep: 3 });
  h.conversation.run();
  await flush();
  assert.deepEqual(h.conversation.entries.map(entryKey), ['3-0', '4-0', '5-0']);
});

test('errors back off and never spin hot, and a success clears the error', async () => {
  const unavailable = Object.assign(new Error('no orchestrator'), { code: 'unavailable' });
  const h = harness([unavailable, unavailable, page([entry(1)], 1)]);
  h.conversation.run();
  await flush();
  assert.equal(h.calls.length, 1);
  assert.equal(h.errors[0], unavailable);
  assert.equal(h.pending().length, 1);
  assert.equal(h.pending()[0].ms, 2_000);
  await flush();
  assert.equal(h.calls.length, 1, 'nothing is requested while backing off');
  await h.fire(2_000);
  assert.equal(h.calls.length, 2);
  assert.equal(h.pending()[0].ms, 5_000, 'the second failure waits longer');
  await h.fire(5_000);
  assert.equal(h.errors.at(-1), null, 'the error clears once a page arrives');
  assert.equal(h.conversation.cursor, 1);
});

test('a poll that returns at once with nothing new waits before the next', async () => {
  const h = harness([page([entry(1)], 1), page([], 1)]);
  h.conversation.run();
  await flush();
  assert.equal(h.calls.length, 2);
  assert.equal(h.pending()[0].ms, MIN_IDLE_POLL_MS);
  await h.fire(MIN_IDLE_POLL_MS);
  assert.equal(h.calls.length, 3);
});

test('a malformed reply is an error, not data', async () => {
  const h = harness([{ reply: 'feed', data: [] }]);
  h.conversation.run();
  await flush();
  assert.match(h.errors[0].message, /conversation answered feed/);
  assert.equal(h.conversation.entries.length, 0);
});

test('one loop at a time, resumed from the cursor, and a reconnect cuts the backoff short', async () => {
  const failure = new Error('boom');
  const h = harness([page([entry(1)], 1), failure, page([entry(2)], 2)]);
  h.conversation.run();
  h.conversation.run();
  await flush();
  assert.equal(h.calls.length, 2, 'a second run() does not start a second loop');
  h.conversation.run();
  await flush();
  assert.equal(h.calls.length, 4, 'run() while backing off retries at once, then polls on');
  assert.equal(h.calls[2].fields.after, 1, 'the retry resumes from the cursor');
  assert.equal(h.conversation.cursor, 2);
  assert.equal(h.calls[3].fields.after, 2);
});

test('the loop ends quietly when the socket closes', async () => {
  const h = harness([page([entry(1)], 1)]);
  h.conversation.run();
  await flush();
  h.close();
  h.conversation.run();
  assert.equal(h.errors.length, 0);
  assert.equal(h.calls.length, 2);
});

test('the relay mark is stripped only as an exact prefix', () => {
  assert.equal(stripRelayMark(`${WEB_RELAY_MARK}deploy now`), 'deploy now');
  assert.equal(stripRelayMark('deploy now'), 'deploy now');
  assert.equal(stripRelayMark(`say ${WEB_RELAY_MARK}x`), `say ${WEB_RELAY_MARK}x`);
});

test('the conversation needs a request function and the socket state', () => {
  assert.throws(() => createConversation({ isOpen: () => true }), TypeError);
  assert.throws(() => createConversation({ request: () => {} }), TypeError);
});
