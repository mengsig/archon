import test from 'node:test';
import assert from 'node:assert/strict';
import { createInputBatcher, mouseEvent, wheelEvent, modifiersOf } from '../../src/features/design/input.js';

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
  for (let i = 0; i < 100; i += 1) b.push(mouseEvent('move', { x: i, y: i }));
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
