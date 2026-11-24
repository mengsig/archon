import test from 'node:test';
import assert from 'node:assert/strict';
import { createSwitchGate } from '../../src/features/design/switching.js';

function fakeTimers() {
  const due = new Map();
  let next = 1;
  return {
    setTimer: (fn, ms) => { const id = next++; due.set(id, { fn, ms }); return id; },
    clearTimer: (id) => due.delete(id),
    fire() { for (const [id, t] of [...due]) { due.delete(id); t.fn(); } },
    get count() { return due.size; },
  };
}

test('item 9: the old picture is hidden until the new size arrives', () => {
  const t = fakeTimers();
  const seen = [];
  const gate = createSwitchGate({ onChange: (p) => seen.push(p), ...t });
  gate.set({ w: 390, h: 844 });
  gate.set({ w: 390, h: 844 });
  assert.deepEqual(gate.pending, { w: 390, h: 844 });
  gate.set(null);
  assert.equal(gate.pending, null);
  assert.deepEqual(seen, [{ w: 390, h: 844 }, null]);
  assert.equal(t.count, 0, 'no timer left behind');
});

test('a switch that never completes stops hiding the stage after its limit', () => {
  const t = fakeTimers();
  const seen = [];
  const gate = createSwitchGate({ onChange: (p) => seen.push(p), limitMs: 8000, ...t });
  gate.set({ w: 390, h: 844 });
  t.fire();
  assert.equal(gate.pending, null, 'the stage shows the stream again');
  gate.set({ w: 390, h: 844 });
  assert.equal(gate.pending, null, 'every render while the same switch is stuck stays shown');
  gate.set({ w: 1280, h: 800 });
  assert.deepEqual(gate.pending, { w: 1280, h: 800 }, 'a new size hides again');
  gate.set(null);
  gate.set({ w: 390, h: 844 });
  assert.deepEqual(gate.pending, { w: 390, h: 844 }, 'once a switch completes, the next one hides again');
  assert.deepEqual(seen, [{ w: 390, h: 844 }, null, { w: 1280, h: 800 }, null, { w: 390, h: 844 }]);
});
