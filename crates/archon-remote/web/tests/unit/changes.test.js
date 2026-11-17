import test from 'node:test';
import assert from 'node:assert/strict';
import { createChanges } from '../../src/core/changes.js';
import { createTransport } from '../../src/core/transport.js';
import { parallelOffsets } from '../../src/ui/graph/render.js';

test('the change stream subscribes live only, and only where the daemon serves it', async () => {
  const sent = [];
  const changes = createChanges({ request: async (cmd, fields) => { sent.push([cmd, fields]); return { reply: 'subscribed', data: 4 }; } });
  assert.equal(await changes.sync({ features: ['task_graph'] }), null);
  assert.deepEqual(sent, []);
  assert.equal(await changes.sync({ features: ['changes'] }), 4);
  assert.deepEqual(sent, [['subscribe_changes', { after: null }]]);
  await assert.rejects(() => changes.sync({}), /lists its features/);
});

test('a refused subscription is an error, not a silent stream', async () => {
  const changes = createChanges({ request: async () => ({ reply: 'ok' }) });
  await assert.rejects(() => changes.sync({ features: ['changes'] }), /answered ok/);
});

test('pushed changes reach every listener until it unsubscribes', () => {
  const changes = createChanges({ request: async () => ({}) });
  const seen = [];
  const off = changes.subscribe((change) => seen.push(change.subject));
  changes.push({ seq: 1, subject: 'run', run: 'pr_1', item: 'qi_1', state: 'running' });
  off();
  changes.push({ seq: 2, subject: 'item' });
  assert.deepEqual(seen, ['run']);
  assert.throws(() => changes.push({ subject: 'item' }), /seq and a subject/);
});

test('the transport hands change frames to onChange', () => {
  const sockets = [];
  class FakeSocket {
    constructor() { this.sent = []; sockets.push(this); }
    send(text) { this.sent.push(JSON.parse(text)); }
    close() {}
  }
  const received = [];
  const transport = createTransport({
    url: 'ws://host/ws', hello: { protocol_version: 24 }, WebSocketImpl: FakeSocket,
    setTimer: () => ({}), clearTimer: () => {}, onChange: (change, sub) => received.push([change.subject, sub]),
  });
  transport.start();
  sockets[0].onmessage({ data: JSON.stringify({ frame: 'change', sub: 2, change: { seq: 9, subject: 'question', question: 'q_1', open: true } }) });
  assert.deepEqual(received, [['question', 2]]);
});

test('edges that share endpoints shift apart and stack their labels a full label apart', () => {
  const edges = [{ from: 'a', to: 'b', kind: 'depends_on' }, { from: 'a', to: 'b', kind: 'stacked_on' }, { from: 'b', to: 'c', kind: 'depends_on' }];
  const [first, second, alone] = parallelOffsets(edges);
  assert.notEqual(first.shift, second.shift);
  assert.ok(Math.abs(first.labelShift - second.labelShift) >= 26, 'two 13px lines per label never overlap');
  assert.deepEqual(alone, { shift: 0, labelShift: 0 });
});
