import test from 'node:test';
import assert from 'node:assert/strict';
import { createStore } from '../../src/core/store.js';
import { WEB_RELAY_MARK, OPERATOR_POST_LABEL } from '../../src/core/conversation.js';
import { addPending, settlePending, matchEchoes, removePending, commandOutcome } from '../../src/features/chat/outbox.js';
import { daemonCommands } from '../../src/core/daemon.js';

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

test("archond's post of a command it ran settles the send; another post does not", () => {
  const s = store();
  const id = addPending(s, '/status');
  matchEchoes(s, [{ ...operator(7, '/status'), kind: 'post' }]);
  assert.equal(s.get().outbox.length, 1, 'a plain post is no echo');
  matchEchoes(s, [{ ...operator(8, `${OPERATOR_POST_LABEL}/status`), kind: 'post' }]);
  assert.equal(s.get().outbox.length, 0);
  assert.equal(s.get().chatNotes['8-0'].localId, id);
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

const fed = (seq, entry) => ({ seq, at: '2026-11-13T10:00:00Z', entry });
const setLine = '/archon apply {"cmd":"set_directive","text":"docs only"}';

test('uncorrelated feed lines do not claim a command outcome', () => {
  const s = createStore({ conversation: [], outbox: [], chatNotes: {}, feed: [fed(4, { entry: 'system', text: 'old line' })] });
  addPending(s, setLine);
  const pending = s.get().outbox[0];
  const feed = [...s.get().feed,
    fed(5, { entry: 'operator', text: setLine, local: true }),
    fed(6, { entry: 'authority', action: 'set_directive', result: 'requested' }),
    fed(7, { entry: 'system', text: 'Directive dir_1 in force: docs only' }),
    fed(8, { entry: 'system', text: 'Operator command: Applied.' })];
  assert.equal(commandOutcome(feed.slice(0, 4), pending), null, 'not until the result is there');
  assert.equal(commandOutcome(feed, pending), null);
});

test('another command’s confirmation or refusal is never attributed by proximity', () => {
  const pending = { text: '/archon set max_parallel_agents=40', localId: 'pending-id' };
  const asked = [fed(1, { entry: 'operator', text: pending.text }), fed(2, { entry: 'system', text: 'Operator command: Awaiting your confirmation as Q7; nothing changed yet.' })];
  assert.equal(commandOutcome(asked, pending), null);
  const refused = [fed(1, { entry: 'operator', text: pending.text }), fed(2, { entry: 'system', text: 'Operator command refused: no such gate' })];
  assert.equal(commandOutcome(refused, pending), null);
  // An echo from before the send, or a line that is not a command, has no answer here.
  assert.equal(commandOutcome(asked, { ...pending, localId: 'another-id' }), null);
  assert.equal(commandOutcome([fed(1, { entry: 'operator', text: 'hello' }), fed(2, { entry: 'system', text: 'Operator command: Applied.' })], { text: 'hello', localId: 'pending-id' }), null);
});

test('identical concurrent commands match only their own interleaved outcome', () => {
  const s = store();
  const first = addPending(s, '/status');
  const second = addPending(s, '/status');
  assert.notEqual(first, second);
  const feed = [fed(1, { entry: 'system', command_id: second, text: 'second detail' }),
    fed(2, { entry: 'system', command_id: first, text: 'first detail' }),
    fed(3, { entry: 'system', command_id: second, text: 'Operator command: Awaiting your confirmation as Q7; nothing changed yet.' }),
    fed(4, { entry: 'system', text: 'Operator command: uncorrelated' }),
    fed(5, { entry: 'system', command_id: first, text: 'Operator command: Applied.' })];
  assert.deepEqual(commandOutcome(feed, s.get().outbox[0]), { lines: ['first detail', 'Applied.'], question: null });
  assert.deepEqual(commandOutcome(feed, s.get().outbox[1]), { lines: ['second detail', 'Awaiting your confirmation as Q7; nothing changed yet.'], question: 'Q7' });
});

test('say sends the pending command identifier to the daemon', async () => {
  const s = store();
  const id = addPending(s, '/status');
  let request;
  const commands = daemonCommands(async (cmd, fields) => { request = { cmd, fields }; return { reply: 'ok' }; });
  await commands.say('/status', id);
  assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.deepEqual(request, { cmd: 'say', fields: { text: '/status', command_id: id } });
});
