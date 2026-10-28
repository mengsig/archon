import test from 'node:test';
import assert from 'node:assert/strict';
import { prUrlOf } from '../../src/core/format.js';

const stage = (state, prUrl) => ({ state: { state }, structured: prUrl ? { pr_url: prUrl } : null });

test('the pull request is the newest completed stage that names one', () => {
  const detail = {
    stages: [
      stage('completed', 'https://github.com/o/r/pull/1'),
      stage('completed', 'https://github.com/o/r/pull/2'),
      stage('failed', 'https://github.com/o/r/pull/3'),
      stage('completed', null),
    ],
  };
  assert.equal(prUrlOf(detail), 'https://github.com/o/r/pull/2');
  assert.equal(prUrlOf({ stages: [] }), null);
  assert.throws(() => prUrlOf({}), TypeError);
});
