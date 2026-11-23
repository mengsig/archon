import test from 'node:test';
import assert from 'node:assert/strict';
import { summaryCounts } from '../../src/features/queue/queue.js';

test('queue summary reports work units and suppresses incomplete chains', () => {
  const units = { active: 2, finished: 1, missing_targets: 0 };
  assert.deepEqual(summaryCounts(units, []), [[2, 'retained active work units'], [1, 'retained finished work units']]);
  assert.deepEqual(summaryCounts({ ...units, missing_targets: 1 }, []), []);
});

test('older daemons retain labelled loaded-item counts', () => {
  const items = [{ state: { state: 'queued' } }, { state: { state: 'succeeded' } }];
  assert.deepEqual(summaryCounts(null, items), [[1, 'loaded active items'], [1, 'loaded finished items']]);
});
