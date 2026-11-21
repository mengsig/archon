import test from 'node:test';
import assert from 'node:assert/strict';
import { createDesignStream } from '../../src/features/design/stream.js';
import { designCommands, createDesignHub } from '../../src/core/design.js';
import { initialState, reduce } from '../../src/features/design/state.js';

function fake(sessionId = 'ds_1') {
  const calls = [];
  const live = { id: sessionId, state: 'live', note_version: 1 };
  const box = { session: live };
  const ctx = {
    canOperate: true,
    designHub: createDesignHub(),
    store: { get: () => ({ connection: { state: 'open' } }), watch: () => () => {} },
    design: {
      get: async () => { calls.push('get'); return { kind: 'design_state', data: { session: box.session, notes: [] } }; },
      subscribe: async (id) => { calls.push(`subscribe:${id}`); return calls.filter((c) => c.startsWith('subscribe')).length; },
      ack: async () => {},
    },
  };
  return { ctx, calls, box };
}
const make = (ctx) => createDesignStream({ ctx, item: 'qi_1', onView() {}, onEvent() {}, onFrame() {}, onError(e) { throw e; } });

test('note_version changes re-run design_get but keep the one subscription', async () => {
  const { ctx, calls } = fake();
  const stream = make(ctx);
  await stream.start();
  // Session events with rising note_version, each marking the state stale as the screen sees it.
  let state = reduce(initialState('qi_1'), { type: 'view', view: { session: { id: 'ds_1', state: 'live', note_version: 1 }, notes: [] } });
  for (let v = 2; v <= 8; v += 1) {
    state = reduce(state, { type: 'event', event: { event: 'session', state: 'live', degraded: false, note_version: v } });
    assert.equal(state.refresh, true);
    await stream.refresh();
    state = { ...state, refresh: false };
  }
  assert.equal(calls.filter((c) => c.startsWith('subscribe')).length, 1);
  assert.equal(calls.filter((c) => c === 'get').length, 8);
  assert.equal(stream.sub, 1);
  stream.stop();
});

test('a new session id or a start (reconnect) subscribes again', async () => {
  const { ctx, calls, box } = fake();
  const stream = make(ctx);
  await stream.start();
  box.session = { id: 'ds_2', state: 'live', note_version: 1 };
  await stream.refresh();
  assert.deepEqual(calls.filter((c) => c.startsWith('subscribe')), ['subscribe:ds_1', 'subscribe:ds_2']);
  await stream.start();
  assert.equal(calls.filter((c) => c.startsWith('subscribe')).length, 3);
  stream.stop();
});

test('subscribe reads the sub number from data.id', async () => {
  const request = async () => ({ reply: 'subscribed', data: { id: 7, last_seq: 0 } });
  assert.equal(await designCommands(request).subscribe('ds_1'), 7);
  const old = async () => ({ reply: 'subscribed', data: { sub: 7 } });
  await assert.rejects(designCommands(old).subscribe('ds_1'));
});
