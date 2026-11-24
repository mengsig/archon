import test from 'node:test';
import assert from 'node:assert/strict';
import { prUrlOf, namesItem, hostVersionLabel } from '../../src/core/format.js';

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

test('only live notifications, questions and reports about the item name it', () => {
  const live = (entry) => ({ seq: 1, entry });
  assert.equal(namesItem(live({ entry: 'notification', kind: 'progress', item: 'qi_A' }), 'qi_A'), true);
  assert.equal(namesItem(live({ entry: 'question', item: 'qi_A' }), 'qi_A'), true);
  assert.equal(namesItem(live({ entry: 'report', item: 'qi_A' }), 'qi_A'), true);
  assert.equal(namesItem(live({ entry: 'notification', item: 'qi_B' }), 'qi_A'), false);
  assert.equal(namesItem(live({ entry: 'operator', text: 'qi_A' }), 'qi_A'), false);
  assert.equal(namesItem({ ...live({ entry: 'report', item: 'qi_A' }), replayed: true }, 'qi_A'), false);
  assert.throws(() => namesItem(live({ entry: 'report' }), ''), TypeError);
});

test('GitHub references become explicit safe links, never URL-shaped repository text', async () => {
  const { githubRefUrl } = await import('../../src/core/format.js');
  assert.equal(githubRefUrl({ host: 'github.com', repo: 'mengsig/archon', number: 427, kind: 'issue' }), 'https://github.com/mengsig/archon/issues/427');
  assert.equal(githubRefUrl({ host: 'github.com', repo: 'mengsig/archon', number: 428, kind: 'pull_request' }), 'https://github.com/mengsig/archon/pull/428');
  assert.equal(githubRefUrl({ host: 'javascript:alert(1)', repo: 'x/y', number: 1 }), null);
  assert.equal(githubRefUrl({ host: 'github.com', repo: '../evil?x', number: 1 }), null);
});

test('the sidebar shows the running daemon version, not the app one', () => {
  assert.equal(hostVersionLabel({ status: { daemon_version: '1.6.4' }, session: { daemon_version: '1.6.3' } }), 'Archon 1.6.4');
  assert.equal(hostVersionLabel({ status: null, session: { daemon_version: '1.6.4' } }), 'Archon 1.6.4');
  assert.equal(hostVersionLabel({ status: null, session: {} }), 'Archon');
});
