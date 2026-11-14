import test from 'node:test';
import assert from 'node:assert/strict';
import { createStore } from '../../src/core/store.js';
import { WEB_RELAY_MARK } from '../../src/core/conversation.js';
import { addPending, settlePending, matchEchoes, removePending } from '../../src/features/chat/outbox.js';

const store = (conversation = []) => createStore({ conversation, outbox: [], chatNotes: {} });
const operator = (seq, text, part = 0) => ({ seq, part, at: '2026-11-13T10:00:00Z', kind: 'operator', important: true, text });

test('an operator entry replaces the pending send with the same trimmed text', () => {
  const s = store();
  const first = addPending(s, 'ship it');
  addPending(s, 'other line');
  settlePending(s, first, 'sent');
  matchEchoes(s, [operator(12, `${WEB_RELAY_MARK}ship it \n`)]);
  assert.deepEqual(s.get().outbox.map((e) => e.text), ['other line']);
  assert.deepEqual(s.get().chatNotes['12-0'], { localId: first, status: 'sent', error: null });
});

test('the note keeps the outcome of an offline send, and a failed send that echoed counts as sent', () => {
  const s = store();
  const offline = addPending(s, 'a');
  const failed = addPending(s, 'b');
  settlePending(s, offline, 'offline', 'Aether is down');
  settlePending(s, failed, 'failed', 'refused');
  matchEchoes(s, [operator(3, 'a'), operator(4, 'b', 2)]);
  assert.equal(s.get().outbox.length, 0);
  assert.deepEqual(s.get().chatNotes['3-0'], { localId: offline, status: 'offline', error: 'Aether is down' });
  assert.deepEqual(s.get().chatNotes['4-2'], { localId: failed, status: 'sent', error: null });
});

test('other kinds and unmatched text leave the outbox alone', () => {
  const s = store();
  addPending(s, 'hello');
  matchEchoes(s, [{ ...operator(1, 'hello'), kind: 'assistant' }, operator(2, 'hello there')]);
  assert.equal(s.get().outbox.length, 1);
  assert.deepEqual(s.get().chatNotes, {});
});

test('each operator entry takes one pending send, oldest first', () => {
  const s = store();
  const a = addPending(s, 'same');
  const b = addPending(s, 'same');
  matchEchoes(s, [operator(5, 'same')]);
  assert.deepEqual(s.get().outbox.map((e) => e.localId), [b]);
  assert.equal(s.get().chatNotes['5-0'].localId, a);
});

test('an identical older line does not settle a send that began after it', () => {
  const s = store([operator(20, 'ship it')]);
  const id = addPending(s, 'ship it');
  matchEchoes(s, [operator(19, 'ship it'), operator(20, 'ship it')]);
  assert.deepEqual(s.get().outbox.map((e) => e.localId), [id], 'a resync page replays old lines');
  matchEchoes(s, [operator(21, 'ship it')]);
  assert.equal(s.get().outbox.length, 0);
  assert.equal(s.get().chatNotes['21-0'].localId, id);
});

test('a send outcome that arrives after the echo lands on the entry', () => {
  const s = store();
  const id = addPending(s, 'late');
  matchEchoes(s, [operator(8, 'late')]);
  settlePending(s, id, 'offline', 'down');
  assert.equal(s.get().chatNotes['8-0'].status, 'offline');
  removePending(s, id);
  assert.throws(() => matchEchoes(s, null), TypeError);
});
