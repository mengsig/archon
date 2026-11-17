import test from 'node:test';
import assert from 'node:assert/strict';
import { deviceGuard, sessionChecker } from '../../src/core/session-watch.js';
import { canOperate } from '../../src/core/role.js';

const operator = { id: 'dev_1', name: 'phone', role: 'operator' };

function watch(readSession) {
  const events = [];
  const guard = deviceGuard(operator, () => events.push('remount'));
  const check = sessionChecker({
    readSession, guard, stop: () => events.push('stop'), onUnpaired: (note) => events.push(note), minGapMs: 0,
  });
  return { check, events };
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
