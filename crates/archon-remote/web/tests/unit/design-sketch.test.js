import test from 'node:test';
import assert from 'node:assert/strict';
import { createSketch, fitWithin, serialise, MAX_SKETCH_BYTES } from '../../src/features/design/sketch-model.js';

test('a sketch serialises its marks in order, rounded', () => {
  const s = createSketch({ w: 390, h: 844 });
  s.add({ tool: 'pen', points: [[1.234, 2.345], [3, 4]] });
  s.add({ tool: 'rect', x: 10.04, y: 20, w: 30, h: 40 });
  s.add({ tool: 'arrow', x1: 0, y1: 0, x2: 50.55, y2: 50 });
  s.add({ tool: 'text', x: 5, y: 6, text: 'Make this bigger' });
  assert.deepEqual(JSON.parse(s.serialise()), {
    v: 1, w: 390, h: 844, items: [
      { tool: 'pen', points: [[1.2, 2.3], [3, 4]] }, { tool: 'rect', x: 10, y: 20, w: 30, h: 40 },
      { tool: 'arrow', x1: 0, y1: 0, x2: 50.6, y2: 50 }, { tool: 'text', x: 5, y: 6, text: 'Make this bigger' },
    ],
  });
});

test('undo removes the last mark and clear all of them', () => {
  const s = createSketch({ w: 10, h: 10 });
  s.add({ tool: 'rect', x: 0, y: 0, w: 1, h: 1 });
  s.add({ tool: 'rect', x: 0, y: 0, w: 2, h: 2 });
  s.undo();
  assert.equal(s.items.length, 1);
  s.clear();
  assert.equal(s.items.length, 0);
  assert.throws(() => s.add({ tool: 'lasso' }), RangeError);
});

test('an oversize sketch keeps its newest marks within the cap', () => {
  const items = Array.from({ length: 1500 }, (_, i) => ({ tool: 'pen', points: Array.from({ length: 40 }, (_, j) => [i + j / 10, j]) }));
  const json = serialise(items, { w: 1, h: 1 });
  assert.ok(json.length <= MAX_SKETCH_BYTES);
  const kept = JSON.parse(json).items;
  assert.deepEqual(kept.at(-1), items.at(-1));
});

test('images fit within the side cap and keep their shape', () => {
  assert.deepEqual(fitWithin(8192, 4096, 4096), { w: 4096, h: 2048 });
  assert.deepEqual(fitWithin(800, 600, 4096), { w: 800, h: 600 });
});
