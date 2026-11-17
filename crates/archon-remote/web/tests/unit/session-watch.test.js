import test from 'node:test';
import assert from 'node:assert/strict';
import { deviceGuard, sessionChecker } from '../../src/core/session-watch.js';
import { canOperate } from '../../src/core/role.js';

const operator = { id: 'dev_1', name: 'phone', role: 'operator' };

function watch(readSession) {
  const events = [];
  const guard = deviceGuard(operator, () => events.push('remount'));
  const watcher = sessionChecker({
    readSession, guard, stop: () => events.push('stop'), onUnpaired: (note) => events.push(note), minGapMs: 0,
  });
  return { check: watcher.check, cancel: watcher.cancel, events };
}

test('a re-pair as an observer during a disconnect remounts, whether or not the id changed', async () => {
  for (const device of [{ ...operator, role: 'observer' }, { id: 'dev_2', name: 'phone', role: 'observer' }]) {
    const { check, events } = watch(async () => ({ device, protocol_version: 1 }));
    await check();
    await check();
    assert.deepEqual(events, ['remount']);
    assert.equal(canOperate(device), false);
  }
});

test('the same device and role leaves the app alone; an unreachable host is not a change', async () => {
  const same = watch(async () => ({ device: { ...operator, name: 'renamed' } }));
  await same.check();
  const offline = watch(async () => { throw new Error('offline'); });
  await offline.check();
  assert.deepEqual([...same.events, ...offline.events], []);
});

test('an unknown role fails closed and a revoked device unpairs', async () => {
  const odd = watch(async () => ({ device: { ...operator, role: 'admin' } }));
  await odd.check();
  assert.deepEqual(odd.events, ['remount']);
  assert.equal(canOperate({ ...operator, role: 'admin' }), false);
  const gone = watch(async () => null);
  await gone.check();
  assert.deepEqual(gone.events, ['stop', 'This device is no longer paired.']);
});

test('a welcome naming another role is a change too', () => {
  const events = [];
  const guard = deviceGuard(operator, () => events.push('remount'));
  assert.equal(guard({ ...operator }), false);
  assert.equal(guard({ ...operator, role: 'observer' }), true);
  assert.deepEqual(events, ['remount']);
  assert.throws(() => deviceGuard(null, () => {}), /device the app was built for/);
});

test('a stopped app ignores a session read that answers late', async () => {
  for (const answer of [null, { device: { ...operator, role: 'observer' } }]) {
    let settle;
    const { check, cancel, events } = watch(() => new Promise((resolve) => { settle = resolve; }));
    const reading = check();
    cancel();
    settle(answer);
    await reading;
    await check();
    assert.deepEqual(events, [], `a late ${answer ? 'change' : 'unpairing'} reaches no stopped app`);
  }
});

test('a forced check skips the gap that throttles the connection watch', async () => {
  let reads = 0;
  let clock = 0;
  const guard = deviceGuard(operator, () => {});
  const { check } = sessionChecker({
    readSession: async () => { reads += 1; return { device: operator }; },
    guard, stop: () => {}, onUnpaired: () => {}, minGapMs: 10_000, now: () => clock,
  });
  await check();
  clock = 5_000;
  await check();
  assert.equal(reads, 1, 'a second check inside the gap reads nothing');
  await check({ force: true });
  assert.equal(reads, 2, 'another tab\'s pairing or a shown page reads at once');
  assert.throws(() => sessionChecker({ readSession: async () => null, guard, stop() {}, onUnpaired() {}, minGapMs: Number.NaN }), /minGapMs/);
});

const QUICK = Object.freeze({ baseMs: 1, capMs: 4 });
const settled = (ms = 40) => new Promise((resolve) => { setTimeout(resolve, ms); });

function flaky(answers, extra = {}) {
  const events = [];
  let reads = 0;
  const guard = deviceGuard(operator, () => events.push('remount'));
  const watcher = sessionChecker({
    readSession: async () => {
      const answer = answers[Math.min(reads, answers.length - 1)];
      reads += 1;
      if (answer instanceof Error) throw answer;
      return answer;
    },
    guard, stop: () => events.push('stop'), onUnpaired: (note) => events.push(note),
    onUnsure: (unsure) => events.push(unsure ? 'unsure' : 'sure'), minGapMs: 0, limits: QUICK, ...extra,
  });
  return { ...watcher, events, reads: () => reads };
}

test('a pairing signal whose first session read fails retries until the remount', async () => {
  const busy = new Error('busy');
  const { check, cancel, events, reads } = flaky([busy, busy, { device: { ...operator, role: 'observer' } }]);
  await check({ force: true });
  assert.deepEqual(events, [], 'one failed read is no answer');
  await settled();
  assert.equal(reads(), 3, 'the read retried until the host answered');
  assert.deepEqual(events, ['remount']);
  cancel();
});

test('cancel stops the retries of an unanswered session read', async () => {
  const { check, cancel, events, reads } = flaky([new Error('offline')]);
  await check({ force: true });
  cancel();
  const after = reads();
  await settled();
  assert.equal(reads(), after, 'a stopped app reads no session');
  assert.deepEqual(events, []);
});

test('retries that never answer withhold operator controls until an answer comes', async () => {
  const offline = new Error('offline');
  const { check, cancel, events } = flaky([offline, offline, offline, { device: operator }], { unsureAfter: 2 });
  await check({ force: true });
  await settled();
  assert.deepEqual(events, ['unsure', 'sure'], 'the same device and role restores the controls');
  cancel();
  const silent = flaky([offline], { unsureAfter: 0 });
  await silent.check({ force: true });
  await settled(10);
  silent.cancel();
  assert.deepEqual(silent.events, ['unsure']);
  assert.ok(silent.reads() > 1, 'it keeps asking past the point it fails closed');
});
