import test from 'node:test';
import assert from 'node:assert/strict';
import { deviceGuard, sessionChecker } from '../../src/core/session-watch.js';
import { ApiError } from '../../src/core/api.js';
import { canOperate } from '../../src/core/role.js';

const operator = { id: 'dev_1', name: 'phone', role: 'operator' };
const observer = { ...operator, role: 'observer' };
const UNPAIRED = 'This device is no longer paired.';

function watch(readSession) {
  const events = [];
  const guard = deviceGuard(operator, () => events.push('remount'));
  const watcher = sessionChecker({
    readSession, guard, stop: () => events.push('stop'), onUnpaired: (note) => events.push(note), minGapMs: 0,
  });
  return { watcher, events };
}

test('a re-pair as an observer during a disconnect remounts, whether or not the id changed', async () => {
  for (const device of [observer, { id: 'dev_2', name: 'phone', role: 'observer' }]) {
    const { watcher, events } = watch(async () => ({ device, protocol_version: 1 }));
    await watcher.check();
    await watcher.check();
    assert.deepEqual(events, ['remount']);
    assert.equal(canOperate(device), false);
  }
});

test('the same device and role leaves the app alone; an unreachable host is not a change', async () => {
  const same = watch(async () => ({ device: { ...operator, name: 'renamed' } }));
  await same.watcher.check();
  const offline = watch(async () => { throw new Error('offline'); });
  await offline.watcher.check();
  assert.deepEqual([...same.events, ...offline.events], []);
});

test('an unknown role fails closed and a revoked device unpairs', async () => {
  const odd = watch(async () => ({ device: { ...operator, role: 'admin' } }));
  await odd.watcher.check();
  assert.deepEqual(odd.events, ['remount']);
  assert.equal(canOperate({ ...operator, role: 'admin' }), false);
  const gone = watch(async () => null);
  await gone.watcher.check();
  assert.deepEqual(gone.events, ['stop', UNPAIRED]);
});

test('a welcome naming another role is a change too', () => {
  const events = [];
  const guard = deviceGuard(operator, () => events.push('remount'));
  assert.equal(guard({ ...operator }), false);
  assert.equal(guard(observer), true);
  assert.deepEqual(events, ['remount']);
  assert.throws(() => deviceGuard(null, () => {}), /device the app was built for/);
});

test('a shown page reads at once, inside the gap that throttles the connection watch', async () => {
  let reads = 0;
  let clock = 0;
  const guard = deviceGuard(operator, () => {});
  const options = { guard, stop() {}, onUnpaired() {}, minGapMs: 10_000 };
  const watcher = sessionChecker({ ...options, readSession: async () => { reads += 1; return { device: operator }; }, now: () => clock });
  await watcher.check();
  clock = 5_000;
  await watcher.check();
  assert.equal(reads, 1, 'a second check inside the gap reads nothing');
  await watcher.recheck();
  assert.equal(reads, 2);
  assert.throws(() => sessionChecker({ ...options, readSession: async () => null, minGapMs: Number.NaN }), /minGapMs/);
  assert.throws(() => sessionChecker({ ...options, readSession: async () => null, unsureAfter: 0 }), /unsureAfter/);
});

// Every backoff is 50–100 ms, so a 100 ms tick always brings the next retry.
const QUICK = Object.freeze({ baseMs: 100, capMs: 100 });
const UNSURE_AFTER = 3;
const flush = () => new Promise((resolve) => { setImmediate(resolve); });

// A checker on fake timers whose session reads the test settles by hand, in the order they began.
function harness(t, extra = {}) {
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const events = [];
  const reads = [];
  const guard = deviceGuard(operator, () => events.push('remount'));
  const watcher = sessionChecker({
    readSession: () => new Promise((resolve, reject) => { reads.push({ resolve, reject }); }),
    guard, stop: () => events.push('stop'), onUnpaired: (note) => events.push(note),
    onUnsure: (unsure) => events.push(unsure ? 'unsure' : 'sure'), minGapMs: 0, limits: QUICK, unsureAfter: UNSURE_AFTER, ...extra,
  });
  return {
    watcher, events, reads,
    async answer(index, session) {
      reads[index].resolve(session);
      await flush();
    },
    async fail(index, error = new ApiError('network', 'The host cannot be reached.', 0)) {
      reads[index].reject(error);
      await flush();
    },
    async tick(ms) {
      t.mock.timers.tick(ms);
      await flush();
    },
  };
}

test('a pairing signal withholds the controls before its first read answers, through every failure', async (t) => {
  const { watcher, events, reads, answer, fail, tick } = harness(t);
  watcher.signal();
  assert.deepEqual(events, ['unsure'], 'withheld before any read resolved');
  assert.equal(reads.length, 1, 'and the session is being read');
  for (let failed = 1; failed <= UNSURE_AFTER + 2; failed += 1) {
    await fail(failed - 1);
    await tick(100);
    assert.equal(reads.length, failed + 1, `failure ${failed} is retried`);
    assert.deepEqual(events, ['unsure'], `still withheld after ${failed} failed reads`);
  }
  watcher.signal();
  assert.deepEqual(events, ['unsure'], 'a repeated signal lifts nothing');
  await answer(reads.length - 1, { device: operator });
  assert.deepEqual(events, ['unsure', 'sure'], 'the same device and role restores the controls');
  await tick(10_000);
  assert.equal(reads.length, UNSURE_AFTER + 4, 'an answered round reads no more');
});

test('after a pairing signal a changed device remounts without restoring the controls', async (t) => {
  const { watcher, events, answer } = harness(t);
  watcher.signal();
  await answer(0, { device: observer });
  assert.deepEqual(events, ['unsure', 'remount']);
});

test('after a pairing signal a 401 unpairs without restoring the controls', async (t) => {
  const { watcher, events, reads, answer, fail, tick } = harness(t);
  watcher.signal();
  await fail(0, new ApiError('busy', 'Every connection slot is taken.', 503));
  await tick(100);
  await answer(1, null);
  assert.deepEqual(events, ['unsure', 'stop', UNPAIRED]);
  await watcher.signal();
  assert.equal(reads.length, 2, 'an unpaired checker is stopped');
});

test('a shown page is unsure after exactly unsureAfter failed reads', async (t) => {
  const { watcher, events, reads, answer, fail, tick } = harness(t);
  watcher.recheck();
  assert.deepEqual(events, [], 'a shown page withholds nothing at once');
  for (let failed = 1; failed < UNSURE_AFTER; failed += 1) {
    await fail(failed - 1);
    assert.deepEqual(events, [], `${failed} failed reads are too few`);
    await tick(100);
  }
  await fail(UNSURE_AFTER - 1);
  assert.deepEqual(events, ['unsure'], 'the read that just failed counts');
  await tick(100);
  await answer(reads.length - 1, { device: operator });
  assert.deepEqual(events, ['unsure', 'sure']);
});

test('showing the page again does not reset the failed reads it counts', async (t) => {
  const { watcher, events, reads, fail } = harness(t);
  for (let failed = 1; failed <= UNSURE_AFTER; failed += 1) {
    watcher.recheck();
    assert.equal(reads.length, failed, 'a shown page reads at once, not after the backoff');
    watcher.recheck();
    assert.equal(reads.length, failed, 'and never beside a retrying read still in flight');
    assert.deepEqual(events, []);
    await fail(failed - 1);
  }
  assert.deepEqual(events, ['unsure'], `unsure after ${UNSURE_AFTER} failed reads, however often it was shown`);
});

test('a busy host that names its Retry-After is not read again sooner', async (t) => {
  const { watcher, reads, fail, tick } = harness(t);
  watcher.signal();
  await fail(0, new ApiError('busy', 'Every connection slot is taken.', 503, 2));
  await tick(1_999);
  assert.equal(reads.length, 1, 'no read inside the 2 s the host asked for');
  // The jitter adds at most half the 100 ms cap.
  await tick(51);
  assert.equal(reads.length, 2);
  await fail(1);
  await tick(100);
  assert.equal(reads.length, 3, 'a failure that names no wait backs off as usual');
});

test('an answer to an unforced read ends the round: its stale retry changes nothing', async (t) => {
  const { watcher, events, reads, answer, fail, tick } = harness(t, { unsureAfter: 2 });
  watcher.recheck();
  await fail(0);
  watcher.check();
  await tick(100);
  assert.equal(reads.length, 3, 'the unforced read and the retry are both in flight');
  await answer(1, { device: operator });
  await fail(2);
  await tick(10_000);
  assert.equal(reads.length, 3, 'the failed stale retry is not retried');
  assert.deepEqual(events, [], 'and counts toward nothing');
  watcher.recheck();
  await fail(3);
  assert.deepEqual(events, [], 'the answer zeroed the count');
});

test('an answer to an unforced read drops the retry that was waiting', async (t) => {
  const { watcher, events, reads, answer, fail, tick } = harness(t);
  watcher.signal();
  await fail(0);
  watcher.check();
  await answer(1, { device: operator });
  assert.deepEqual(events, ['unsure', 'sure']);
  await tick(10_000);
  assert.equal(reads.length, 2, 'no retry outlives the answer');
});

test('a read begun before a newer pairing signal cannot answer for it', async (t) => {
  const { watcher, events, reads, answer, fail, tick } = harness(t);
  watcher.signal();
  watcher.check();
  watcher.signal();
  await fail(2, new ApiError('busy', 'Every connection slot is taken.', 503));
  await answer(0, { device: operator });
  await answer(1, { device: operator });
  assert.deepEqual(events, ['unsure'], 'the replaced operator session confirms nothing');
  await tick(100);
  assert.equal(reads.length, 4, 'the newer signal keeps retrying');
  await answer(3, { device: observer });
  assert.deepEqual(events, ['unsure', 'remount']);
});

test('cancel stops a retry that was waiting', async (t) => {
  const { watcher, events, reads, fail, tick } = harness(t);
  watcher.signal();
  await fail(0);
  watcher.cancel();
  await tick(10_000);
  assert.equal(reads.length, 1, 'the waiting retry never reads');
  assert.deepEqual(events, ['unsure']);
});

for (const [what, late] of [['unpairing', null], ['changed device', { device: observer }], ['confirmation', { device: operator }]]) {
  test(`cancel voids reads in flight and every later event: a late ${what}`, async (t) => {
    const { watcher, events, reads, answer, fail, tick } = harness(t);
    watcher.signal();
    watcher.check();
    watcher.cancel();
    await answer(0, late);
    await fail(1);
    await tick(10_000);
    await Promise.all([watcher.signal(), watcher.recheck(), watcher.check()]);
    assert.equal(reads.length, 2, 'a stopped checker reads no session');
    assert.deepEqual(events, ['unsure'], 'nothing reaches a stopped app');
  });
}
