import test from 'node:test';
import assert from 'node:assert/strict';
import { coalesce, liveQuery, pollStatus, MIN_POLL_MS } from '../../src/core/live-query.js';
import { createStore } from '../../src/core/store.js';

function fakeDocument() {
  const listeners = new Set();
  return {
    visibilityState: 'visible',
    addEventListener: (type, fn) => { assert.equal(type, 'visibilitychange'); listeners.add(fn); },
    removeEventListener: (_type, fn) => listeners.delete(fn),
    show() { this.visibilityState = 'visible'; for (const fn of listeners) fn(); },
    listeners,
  };
}

function fakeContext() {
  const subscribers = { feed: new Set(), changes: new Set() };
  const stream = (set) => ({ subscribe: (fn) => { set.add(fn); return () => set.delete(fn); } });
  return {
    ctx: { feed: stream(subscribers.feed), changes: stream(subscribers.changes),
      store: createStore({ connection: { state: 'open' } }) },
    announce: () => { for (const fn of subscribers.feed) fn(); },
  };
}

test('coalesce runs once per burst and flush runs a pending call at once', (t) => {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  let calls = 0;
  const schedule = coalesce(() => { calls += 1; }, 350);
  schedule();
  schedule();
  assert.equal(schedule.pending(), true);
  t.mock.timers.tick(349);
  assert.equal(calls, 0);
  schedule.flush();
  assert.equal(calls, 1);
  assert.equal(schedule.pending(), false);
  t.mock.timers.tick(1000);
  assert.equal(calls, 1, 'the flushed timer must not fire again');
  assert.throws(() => coalesce(() => {}, -1), RangeError);
});

test('showing the tab reads at once and cancels the burst it was holding', async (t) => {
  t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  globalThis.document = fakeDocument();
  const { ctx, announce } = fakeContext();
  let reads = 0;
  const query = liveQuery(ctx, async () => { reads += 1; return reads; }, () => {});
  await Promise.resolve();
  assert.equal(reads, 1, 'the first read runs on start');
  announce();
  document.show();
  await Promise.resolve();
  assert.equal(reads, 2, 'visible reads without waiting for the coalesce delay');
  t.mock.timers.tick(1000);
  await Promise.resolve();
  assert.equal(reads, 2, 'the held burst read was cancelled, not run twice');
  query.destroy();
  assert.equal(document.listeners.size, 0);
  delete globalThis.document;
});

test('a shown pause state reads status once per 5 s, only while shown and connected', (t) => {
  t.mock.timers.enable({ apis: ['setInterval'] });
  globalThis.document = fakeDocument();
  const { ctx } = fakeContext();
  const reads = [];
  ctx.refresh = { one: (name) => { reads.push(name); return Promise.resolve(true); } };
  const stop = pollStatus(ctx);
  t.mock.timers.tick(MIN_POLL_MS - 1);
  assert.deepEqual(reads, [], 'no read before the interval');
  t.mock.timers.tick(1);
  assert.deepEqual(reads, ['status']);
  t.mock.timers.tick(MIN_POLL_MS * 2);
  assert.equal(reads.length, 3, 'at most one read per interval');
  document.visibilityState = 'hidden';
  t.mock.timers.tick(MIN_POLL_MS);
  document.visibilityState = 'visible';
  ctx.store.set({ connection: { state: 'waiting' } });
  t.mock.timers.tick(MIN_POLL_MS);
  assert.equal(reads.length, 3, 'a hidden or disconnected tab reads nothing');
  ctx.store.set({ connection: { state: 'open' } });
  stop();
  t.mock.timers.tick(MIN_POLL_MS * 3);
  assert.equal(reads.length, 3, 'a left screen reads nothing');
  assert.throws(() => pollStatus(ctx, { interval: 1000 }), RangeError);
  assert.throws(() => pollStatus({ store: ctx.store }), TypeError);
});
