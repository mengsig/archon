import test from 'node:test';
import assert from 'node:assert/strict';
import { createInputBatcher, mouseEvent, wheelEvent, modifiersOf, BUSY_AFTER_MS } from '../../src/features/design/input.js';

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

// A send whose replies the test answers by hand.
function slowSend() {
  const batches = [];
  const replies = [];
  return {
    batches, replies,
    send: (events) => { batches.push(events); return new Promise((resolve) => replies.push(resolve)); },
    answer: async () => { replies.shift()(); await Promise.resolve(); await Promise.resolve(); },
  };
}

test('at most two input requests await a reply; meanwhile wheel ticks add up and moves keep the latest', async () => {
  const c = clock();
  const s = slowSend();
  const b = createInputBatcher({ send: s.send, ...c });
  for (let i = 0; i < 300; i += 1) {
    b.push(wheelEvent({ x: 5, y: 5 }, 1, 10));
    b.push(mouseEvent('move', { x: i, y: i }, { button: null }));
    c.advance(4); // a fast trackpad: 250 events a second for 1.2 s
  }
  assert.equal(s.batches.length, 2, 'no third request before a reply');
  assert.equal(b.outstanding, 2);
  await s.answer();
  c.advance(20);
  assert.equal(s.batches.length, 3);
  const caughtUp = s.batches[2];
  const sent = s.batches.slice(0, 2).flat();
  assert.equal(caughtUp.length, 2, 'one summed wheel and the latest move');
  const wheels = [...sent, ...caughtUp].filter((e) => e.type === 'wheel');
  assert.equal(wheels.reduce((sum, e) => sum + e.dy, 0), 3000, 'no scroll distance is lost');
  assert.equal(wheels.reduce((sum, e) => sum + e.dx, 0), 300);
  assert.deepEqual(caughtUp.at(-1), mouseEvent('move', { x: 299, y: 299 }, { button: null }));
});

test('waiting on replies shows one quiet busy signal that clears when input flows again', async () => {
  const c = clock();
  const s = slowSend();
  const busy = [];
  const b = createInputBatcher({ send: s.send, onBusy: (on) => busy.push(on), ...c });
  b.push({ type: 'text', text: 'a' });
  c.advance(20);
  b.push({ type: 'text', text: 'b' });
  c.advance(20);
  b.push({ type: 'text', text: 'c' });
  c.advance(BUSY_AFTER_MS - 50);
  assert.deepEqual(busy, [], 'a short wait says nothing');
  for (let i = 0; i < 10; i += 1) { b.push({ type: 'text', text: String(i) }); c.advance(10); }
  c.advance(100);
  assert.deepEqual(busy, [true], 'said once, however much input waits');
  await s.answer();
  c.advance(20);
  assert.deepEqual(busy, [true, false]);
  assert.equal(s.batches.length, 3);
});

test('a stop forgets outstanding replies, so late ones cannot unbalance the count', async () => {
  const c = clock();
  const s = slowSend();
  const b = createInputBatcher({ send: s.send, ...c });
  b.push({ type: 'text', text: 'a' }); c.advance(20);
  b.push({ type: 'text', text: 'b' }); c.advance(20);
  b.stop();
  assert.equal(b.outstanding, 0);
  await s.answer();
  await s.answer();
  assert.equal(b.outstanding, 0);
  b.push({ type: 'text', text: 'c' }); c.advance(20);
  assert.equal(b.outstanding, 1);
  assert.equal(s.batches.length, 3);
});
