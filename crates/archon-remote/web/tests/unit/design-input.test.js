import test from 'node:test';
import assert from 'node:assert/strict';
import { createInputBatcher, mouseEvent, wheelEvent, modifiersOf, BUSY_AFTER_MS, RETRY_MS, MAX_RETRIES } from '../../src/features/design/input.js';

function clock() {
  let t = 0;
  const timers = [];
  return {
    now: () => t,
    setTimer: (fn, ms) => { const timer = { fn, at: t + ms }; timers.push(timer); return timer; },
    clearTimer: (timer) => { const i = timers.indexOf(timer); if (i >= 0) timers.splice(i, 1); },
    advance(ms) {
      const end = t + ms;
      for (;;) {
        timers.sort((a, b) => a.at - b.at);
        const next = timers[0];
        if (!next || next.at > end) break;
        timers.shift();
        t = Math.max(t, next.at);
        next.fn();
      }
      t = end;
    },
  };
}

test('moves merge and a batch never exceeds 64 events', () => {
  const c = clock();
  const sent = [];
  const b = createInputBatcher({ send: (e) => sent.push(e), ...c });
  for (let i = 0; i < 100; i += 1) b.push(mouseEvent('move', { x: i, y: i }, { button: null }));
  c.advance(1);
  assert.equal(sent.length, 1);
  assert.equal(sent[0].length, 1, 'consecutive moves merged into the last one');
  assert.equal(sent[0][0].x, 99);
  for (let i = 0; i < 150; i += 1) b.push({ type: 'key', action: i % 2 ? 'up' : 'down', key: 'a', code: 'KeyA', text: null, modifiers: 0 });
  c.advance(1000);
  assert.ok(sent.every((s) => s.length <= 64));
  assert.equal(sent.flat().length, 151);
});

test('no more than 60 requests per second', () => {
  const c = clock();
  const sent = [];
  const b = createInputBatcher({ send: (e) => sent.push(c.now()), ...c });
  for (let i = 0; i < 500; i += 1) {
    b.push({ type: 'text', text: String(i) });
    c.advance(1);
  }
  c.advance(1000);
  for (let w = 0; w + 1000 <= 2000; w += 100) {
    assert.ok(sent.filter((t) => t >= w && t < w + 1000).length <= 60, `window at ${w}`);
  }
  assert.ok(sent.length > 10);
});

test('wheel ticks at one point add up; clicks are never merged away', () => {
  const c = clock();
  const sent = [];
  const b = createInputBatcher({ send: (e) => sent.push(e), ...c });
  b.push(wheelEvent({ x: 5, y: 5 }, 0, 10));
  b.push(wheelEvent({ x: 5, y: 5 }, 0, 15));
  b.push(mouseEvent('down', { x: 1, y: 1 }, { button: 0, clickCount: 1 }));
  b.push(mouseEvent('up', { x: 1, y: 1 }, { button: 0, clickCount: 1 }));
  c.advance(5);
  assert.deepEqual(sent[0].map((e) => e.type + (e.dy ?? e.action)), ['wheel25', 'mousedown', 'mouseup']);
});

test('modifier bits follow CDP', () => {
  assert.equal(modifiersOf({ altKey: true, ctrlKey: true, metaKey: true, shiftKey: true }), 15);
  assert.equal(modifiersOf({ shiftKey: true }), 8);
});

// The exact JSON of crates/archon-core/src/protocol/v25_tests.rs (design_input events).
test('every event kind serializes to the Rust InputEvent shape', async () => {
  const { textEvent, touchEvent } = await import('../../src/features/design/input.js');
  const { keyEvent } = await import('../../src/features/design/keymap.js');
  const rust = [
    { type: 'mouse', action: 'down', x: 1.0, y: 2.0, button: 'left', click_count: 1, modifiers: 0 },
    { type: 'wheel', x: 1.0, y: 2.0, dx: 0.0, dy: 5.0, modifiers: 2 },
    { type: 'key', action: 'up', key: 'a', code: 'KeyA', text: null, modifiers: 0 },
    { type: 'text', text: 'hi' },
    { type: 'touch', action: 'cancel', points: [{ x: 1.0, y: 2.0, id: 3 }] },
  ];
  const web = [
    mouseEvent('down', { x: 1, y: 2 }, { button: 0, clickCount: 1 }),
    wheelEvent({ x: 1, y: 2 }, 0, 5, 2),
    keyEvent('up', { key: 'a', code: 'KeyA', ctrlKey: false, metaKey: false, altKey: false, shiftKey: false }),
    textEvent('hi'),
    touchEvent('cancel', [{ x: 1, y: 2, id: 3 }]),
  ];
  assert.equal(JSON.stringify(web), JSON.stringify(rust));
});

test('saturation preserves releases for all admitted presses and reports skipped input', () => {
  const c = clock();
  const sent = [];
  let overflow = 0;
  const b = createInputBatcher({ send: (events) => sent.push(...events), onOverflow: () => { overflow += 1; }, ...c });
  const key = (action, code = 'KeyA') => ({ type: 'key', action, code, key: code.at(-1).toLowerCase(), text: null, modifiers: 0 });
  const point = { x: 1, y: 2 };
  const down = [key('down'), mouseEvent('down', point), { type: 'touch', action: 'start', points: [point] }];
  const up = [key('up'), mouseEvent('up', point), { type: 'touch', action: 'cancel', points: [point] }];
  for (const event of down) b.push(event);
  b.flush();
  for (let i = 0; i < 600; i += 1) b.push({ type: 'text', text: String(i) });
  assert.equal(b.pending, 509);
  assert.equal(b.push(key('down', 'KeyB')), false);
  assert.equal(b.push(key('up', 'KeyB')), false);
  for (const event of up) assert.equal(b.push(event), true);
  assert.equal(b.pending, 512);
  assert.equal(overflow, 1);
  c.advance(1000);
  assert.equal(b.pending, 0);
  assert.deepEqual(sent.slice(0, 3), down);
  assert.deepEqual(sent.slice(-3), up);
  assert.equal(sent.filter((e) => e.code === 'KeyB').length, 0);
});

// A send whose replies the test answers, or refuses, by hand.
function slowSend() {
  const batches = [];
  const replies = [];
  const settle = async (how) => { replies.shift()[how](new Error('refused')); for (let i = 0; i < 3; i += 1) await Promise.resolve(); };
  return {
    batches, replies,
    send: (events) => { batches.push(events); return new Promise((resolve, reject) => replies.push({ resolve, reject })); },
    answer: () => settle('resolve'),
    refuse: () => settle('reject'),
  };
}

test('one input request awaits a reply at a time; meanwhile wheel ticks at a point add up', async () => {
  const c = clock();
  const s = slowSend();
  const b = createInputBatcher({ send: s.send, ...c });
  for (let i = 0; i < 300; i += 1) {
    b.push(wheelEvent({ x: 5, y: 5 }, 1, 10));
    c.advance(4); // a fast trackpad: 250 ticks a second for 1.2 s, the pointer still
  }
  assert.equal(s.batches.length, 1, 'no second request before the reply');
  assert.equal(b.outstanding, 1);
  await s.answer();
  c.advance(20);
  assert.equal(s.batches.length, 2);
  assert.equal(s.batches[1].length, 1, 'the waiting ticks went as one summed wheel');
  const wheels = s.batches.flat();
  assert.equal(wheels.reduce((sum, e) => sum + e.dy, 0), 3000, 'no scroll distance is lost');
  assert.equal(wheels.reduce((sum, e) => sum + e.dx, 0), 300);
});

test('only back-to-back events merge, so order and wheel targets stay as they were', async () => {
  const c = clock();
  const s = slowSend();
  const b = createInputBatcher({ send: s.send, ...c });
  b.push({ type: 'text', text: 'hold' }); c.advance(1);
  const inside = { x: 100, y: 100 };
  const outside = { x: 100, y: 99 }; // one pixel across a nested panel's edge
  b.push(wheelEvent(inside, 0, 10));
  b.push(wheelEvent(inside, 0, 10));
  b.push(wheelEvent(outside, 0, 10));
  b.push(mouseEvent('move', { x: 1, y: 1 }, { button: null }));
  b.push(mouseEvent('move', { x: 2, y: 2 }, { button: null }));
  b.push(wheelEvent(outside, 0, 10));
  await s.answer();
  c.advance(20);
  assert.deepEqual(s.batches[1], [
    wheelEvent(inside, 0, 20), wheelEvent(outside, 0, 10),
    mouseEvent('move', { x: 2, y: 2 }, { button: null }), wheelEvent(outside, 0, 10),
  ]);
});

test('a refused batch goes again first, in order, so a release is never lost', async () => {
  const c = clock();
  const s = slowSend();
  const busy = [];
  const b = createInputBatcher({ send: s.send, onBusy: (on) => busy.push(on), ...c });
  const down = mouseEvent('down', { x: 1, y: 1 }, { button: 0, clickCount: 1 });
  const up = mouseEvent('up', { x: 1, y: 1 }, { button: 0, clickCount: 1 });
  b.push(down); c.advance(1);
  b.push(up);
  await s.refuse();
  c.advance(RETRY_MS - 1);
  assert.equal(s.batches.length, 1, 'a refusal waits before trying again');
  assert.ok(busy.at(-1), 'a refusal says the preview is behind');
  c.advance(1);
  assert.deepEqual(s.batches[1], [down, up], 'the refused press goes before its release');
  await s.answer();
  assert.equal(b.pending, 0);
  assert.deepEqual(busy, [true, false]);
});

test('a batch refused again and again is given up and reported once', async () => {
  const c = clock();
  const s = slowSend();
  let overflows = 0;
  const b = createInputBatcher({ send: s.send, onOverflow: () => { overflows += 1; }, ...c });
  b.push({ type: 'text', text: 'a' }); c.advance(1);
  for (let i = 0; i < MAX_RETRIES; i += 1) { await s.refuse(); c.advance(RETRY_MS); }
  assert.equal(s.batches.length, MAX_RETRIES + 1);
  await s.refuse();
  c.advance(RETRY_MS * 4);
  assert.equal(overflows, 1);
  assert.equal(s.batches.length, MAX_RETRIES + 1, 'nothing is sent again after giving up');
});

test('waiting on a reply shows one quiet busy signal that clears when input flows again', async () => {
  const c = clock();
  const s = slowSend();
  const busy = [];
  const b = createInputBatcher({ send: s.send, onBusy: (on) => busy.push(on), ...c });
  b.push({ type: 'text', text: 'a' });
  c.advance(20);
  b.push({ type: 'text', text: 'b' });
  c.advance(BUSY_AFTER_MS - 50);
  assert.deepEqual(busy, [], 'a short wait says nothing');
  for (let i = 0; i < 10; i += 1) { b.push({ type: 'text', text: String(i) }); c.advance(10); }
  c.advance(100);
  assert.deepEqual(busy, [true], 'said once, however much input waits');
  await s.answer();
  c.advance(20);
  assert.deepEqual(busy, [true], 'still behind while the rest is on its way');
  assert.equal(s.batches.length, 2);
  await s.answer();
  assert.deepEqual(busy, [true, false], 'cleared once caught up');
});

test('a stop forgets an outstanding reply, so a late one cannot unbalance the count', async () => {
  const c = clock();
  const s = slowSend();
  const b = createInputBatcher({ send: s.send, ...c });
  b.push({ type: 'text', text: 'a' }); c.advance(20);
  b.stop();
  assert.equal(b.outstanding, 0);
  await s.refuse();
  assert.equal(b.pending, 0, 'a late refusal does not requeue what a stop dropped');
  b.push({ type: 'text', text: 'c' }); c.advance(20);
  assert.equal(b.outstanding, 1);
  assert.equal(s.batches.length, 2);
});

test('a release still gets in after a refused press is put back into a full queue', async () => {
  const c = clock();
  const s = slowSend();
  const b = createInputBatcher({ send: s.send, ...c });
  const key = (action) => ({ type: 'key', action, key: 'a', code: 'KeyA', text: null, modifiers: 0 });
  b.push(key('down')); c.advance(1);
  let admitted = 0;
  while (b.push({ type: 'text', text: 'x' })) admitted += 1;
  assert.ok(admitted > 400, 'the queue filled up to its release reserve');
  await s.refuse();
  assert.equal(b.push(key('up')), true, 'the release of a press archond will get is never refused');
  c.advance(RETRY_MS);
  for (let i = 0; i < 20 && s.replies.length > 0; i += 1) { await s.answer(); c.advance(20); }
  const keys = s.batches.slice(1).flat().filter((e) => e.type === 'key');
  assert.deepEqual(keys.map((e) => e.action), ['down', 'up'], 'the press goes again, then its release');
});
