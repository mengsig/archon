import test from 'node:test';
import assert from 'node:assert/strict';
import { coalesce, liveQuery } from '../../src/core/live-query.js';
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
