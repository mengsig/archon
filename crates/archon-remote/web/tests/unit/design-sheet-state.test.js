import test from 'node:test';
import assert from 'node:assert/strict';
import { sheetAfterCancel, readScroll, restoreScroll } from '../../src/features/design/sheet-state.js';

test('Cancel collapses the notes sheet on phone and tablet, leaves desktop alone', () => {
  assert.equal(sheetAfterCancel('phone', 'half'), 'peek');
  assert.equal(sheetAfterCancel('tablet', 'full'), 'peek');
  assert.equal(sheetAfterCancel('desktop', 'half'), 'half');
});

test('the page scroll is read and put back', () => {
  const calls = [];
  const win = { scrollX: 0, scrollY: 0, document: { scrollingElement: { scrollLeft: 3, scrollTop: 40, scrollTo: (x, y) => calls.push([x, y]) } } };
  const pos = readScroll(win);
  assert.deepEqual(pos, { x: 3, y: 40 });
  restoreScroll({ x: 0, y: 0 }, win);
  restoreScroll(pos, win);
  assert.deepEqual(calls, [[0, 0], [3, 40]]);
});
