import test from 'node:test';
import assert from 'node:assert/strict';
import { fitScale, toApp, effectiveScale, normalizeDrag, classifyTwoFinger, clampZoom, boxToScreen } from '../../src/features/design/scale.js';

const frame = { w: 1440, h: 900 };

test('fit scales a large preset down and never up', () => {
  assert.equal(fitScale({ frame, avail: { w: 720, h: 900 } }), 0.5);
  assert.equal(fitScale({ frame: { w: 390, h: 844 }, avail: { w: 1200, h: 2000 } }), 1);
  assert.equal(fitScale({ frame, avail: { w: 720, h: 900 }, mode: 'actual' }), 1);
  assert.equal(fitScale({ frame: { w: 390, h: 844 }, avail: { w: 780, h: 500 }, mode: 'width' }), 2);
});

test('input coordinates are divided by the displayed scale', () => {
  const rect = { left: 100, top: 50, width: 720, height: 450 };
  assert.equal(effectiveScale(rect, frame), 0.5);
  assert.deepEqual(toApp(460, 275, rect, frame), { x: 720, y: 450 });
  assert.deepEqual(toApp(0, 0, rect, frame), { x: 0, y: 0 }, 'clamped into the viewport');
  assert.deepEqual(boxToScreen({ x: 10, y: 20, w: 100, h: 40 }, 0.5), { x: 5, y: 10, w: 50, h: 20 });
});

test('zoom (local pinch) changes the displayed scale, so input still maps', () => {
  const zoomed = { left: 0, top: 0, width: 1440, height: 900 };
  assert.deepEqual(toApp(720, 450, zoomed, frame), { x: 720, y: 450 });
});

test('a drag normalises to a box inside the viewport', () => {
  assert.deepEqual(normalizeDrag({ x: 300, y: 200 }, { x: 100, y: 50 }, frame), { x: 100, y: 50, w: 200, h: 150 });
  assert.deepEqual(normalizeDrag({ x: 1400, y: 10 }, { x: 2000, y: 20 }, frame), { x: 1400, y: 10, w: 40, h: 10 });
});

test('two fingers either pinch or scroll', () => {
  const a = [{ x: 0, y: 0 }, { x: 100, y: 0 }];
  assert.equal(classifyTwoFinger(a, [{ x: -20, y: 0 }, { x: 120, y: 0 }]).kind, 'zoom');
  const pan = classifyTwoFinger(a, [{ x: 0, y: 30 }, { x: 100, y: 30 }]);
  assert.deepEqual([pan.kind, pan.dy], ['pan', 30]);
  assert.equal(clampZoom(9), 4);
  assert.equal(clampZoom(0.2), 1);
});

test('wheel deltas become app pixels: screen pixels follow the scale, lines and pages do not', async () => {
  const { wheelDeltas, WHEEL_LINE_PX } = await import('../../src/features/design/scale.js');
  assert.deepEqual(wheelDeltas({ deltaMode: 0, deltaX: 0.4, deltaY: 50 }, 0.5, 900), { dx: 0.8, dy: 100 }, 'a trackpad keeps its fractions');
  assert.deepEqual(wheelDeltas({ deltaMode: 1, deltaX: 0, deltaY: 3 }, 0.5, 900), { dx: 0, dy: 3 * WHEEL_LINE_PX });
  assert.deepEqual(wheelDeltas({ deltaMode: 2, deltaX: 0, deltaY: -1 }, 2, 900), { dx: 0, dy: -900 });
  assert.throws(() => wheelDeltas({ deltaMode: 0, deltaY: 1 }, 0, 900), RangeError);
});
