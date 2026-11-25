import test from 'node:test';
import assert from 'node:assert/strict';
import { compareLayout } from '../../src/features/design/compare.js';

test('a note with no target compares as a sketch beside the after picture, no slider', () => {
  assert.deepEqual(compareLayout({ id: 'D1', kind: 'page', target: null }), { mode: 'pair', beforeWord: 'Your sketch' });
  assert.deepEqual(compareLayout({ id: 'D1' }), { mode: 'pair', beforeWord: 'Your sketch' });
});

test('a note on an element keeps the before/after slider', () => {
  assert.deepEqual(compareLayout({ id: 'D2', kind: 'change', target: { box: { x: 0, y: 0, w: 1, h: 1 } } }), { mode: 'slider', beforeWord: 'Before' });
});
