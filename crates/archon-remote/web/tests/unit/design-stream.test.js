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

test('a refresh while a subscribe is in flight joins it: one subscription, stored once', async () => {
  const { ctx, calls } = fake();
  let release;
  ctx.design.subscribe = (id) => { calls.push(`subscribe:${id}`); return new Promise((r) => { release = () => r(5); }); };
  const stream = make(ctx);
  const first = stream.start();
  await new Promise((r) => setTimeout(r, 5));
  const second = stream.refresh();
  await new Promise((r) => setTimeout(r, 5));
  release();
  await Promise.all([first, second]);
  assert.equal(calls.filter((c) => c.startsWith('subscribe')).length, 1);
  assert.equal(stream.sub, 5);
  stream.stop();
});

const frame = (seq, jpeg = String(seq)) => ({ event: 'frame', seq, jpeg_b64: jpeg, w: 390, h: 844 });
const tick = () => new Promise((resolve) => setImmediate(resolve));

test('a corrupt frame is acknowledged and the next frame still draws', async () => {
  const { ctx } = fake();
  const acks = [];
  const frames = [];
  let closed = 0;
  ctx.design.ack = async (...args) => { acks.push(args); };
  const stream = createDesignStream({
    ctx, item: 'qi_1', onView() {}, onEvent() {}, onError: assert.fail,
    visible: () => true, onFrame: (_bitmap, meta) => frames.push(meta.seq),
    decode: async (jpeg) => {
      if (jpeg === 'corrupt') throw new Error('Bad JPEG');
      return { close: () => { closed += 1; } };
    },
  });
  await stream.start();
  ctx.designHub.push(1, frame(1, 'corrupt'));
  await tick();
  assert.deepEqual(acks, [[1, 1]]);
  assert.deepEqual(frames, []);
  ctx.designHub.push(1, frame(2));
  await tick();
  assert.deepEqual(acks, [[1, 1], [1, 2]]);
  assert.deepEqual(frames, [2]);
  assert.equal(closed, 1);
  stream.stop();
});

for (const change of ['session', 'connection']) {
  test(`a ${change} change isolates decoded frames and hidden ACKs even with a reused sub id`, async () => {
    const { ctx, box } = fake();
    const acks = [];
    const frames = [];
    const events = [];
    const timers = [];
    let setConnection;
    let release;
    let staleClosed = false;
    ctx.store.watch = (_pick, fn) => { setConnection = fn; return () => {}; };
    ctx.design.subscribe = async () => 1;
    ctx.design.ack = async (...args) => { acks.push(args); };
    const stream = createDesignStream({
      ctx, item: 'qi_1', onView() {}, onError: assert.fail,
      onEvent: (event) => { if (event.event === 'frame') events.push(event.seq); },
      onFrame: (_bitmap, meta) => frames.push(meta.seq), visible: () => false, now: () => 0,
      setTimer: (fn) => { const timer = { fn, cleared: false }; timers.push(timer); return timer; },
      clearTimer: (timer) => { timer.cleared = true; },
      decode: (jpeg) => jpeg === 'slow' ? new Promise((resolve) => { release = resolve; }) : Promise.resolve({}),
    });
    await stream.start();
    ctx.designHub.push(1, frame(20));
    await tick();
    ctx.designHub.push(1, frame(21));
    await tick();
    assert.equal(timers.length, 1);
    ctx.designHub.push(1, frame(22, 'slow'));
    ctx.designHub.push(1, frame(23));
    if (change === 'connection') {
      setConnection('connecting');
      assert.equal(stream.sub, null);
      setConnection('open');
      await tick();
    } else {
      box.session = { id: 'ds_2', state: 'live' };
      await stream.refresh();
    }
    assert.equal(stream.sub, 1);
    assert.equal(timers[0].cleared, true);
    ctx.designHub.push(1, frame(1));
    await tick();
    timers[0].fn();
    release({ close: () => { staleClosed = true; } });
    await tick();
    assert.equal(staleClosed, true);
    assert.deepEqual(acks, [[1, 20], [1, 1]]);
    assert.deepEqual(frames, [20, 21, 1]);
    assert.deepEqual(events, frames);
    stream.stop();
  });
}

test('opening a design while disconnected subscribes when the connection opens', async () => {
  const { ctx, calls } = fake();
  let setConnection;
  ctx.store.get = () => ({ connection: { state: 'connecting' } });
  ctx.store.watch = (_pick, fn) => { setConnection = fn; return () => {}; };
  const stream = make(ctx);
  await stream.start();
  assert.deepEqual(calls, []);
  setConnection('open');
  await tick();
  assert.deepEqual(calls, ['get', 'subscribe:ds_1']);
  assert.equal(stream.sub, 1);
  stream.stop();
});
