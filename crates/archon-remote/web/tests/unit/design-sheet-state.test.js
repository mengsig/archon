import test from 'node:test';
import assert from 'node:assert/strict';
import { sheetAfterCancel, readScroll, restoreScroll } from '../../src/features/design/sheet-state.js';

test('Cancel collapses the notes sheet on phone and tablet, leaves desktop alone', () => {
  assert.equal(sheetAfterCancel('phone', 'half'), 'peek');
  assert.equal(sheetAfterCancel('tablet', 'full'), 'peek');
  assert.equal(sheetAfterCancel('desktop', 'half'), 'half');
  // Escape with no composer open leaves a raised sheet alone.
  assert.equal(sheetAfterCancel('phone', 'full', false), 'full');
});

const scroller = (x, y, calls) => ({ scrollLeft: x, scrollTop: y, scrollTo: (a, b) => calls.push([a, b]) });

test('the page and every scroll container are read and put back', () => {
  const calls = [];
  const page = scroller(3, 40, calls);
  const main = scroller(0, 0, calls);
  const side = { scrollLeft: 0, scrollTop: 12 }; // no scrollTo: set directly
  const win = { document: { scrollingElement: page } };
  const saved = readScroll([main, null, side], win);
  assert.deepEqual(saved.map(({ x, y }) => [x, y]), [[3, 40], [0, 0], [0, 12]]);
  // The composer scrolled the stage column and the sheet away.
  main.scrollTop = 236; side.scrollTop = 327;
  restoreScroll(saved);
  assert.deepEqual(calls, [[3, 40], [0, 0]]);
  assert.equal(side.scrollTop, 12);
  restoreScroll(null); // nothing saved: nothing to do
});
