import test from 'node:test';
import assert from 'node:assert/strict';
import { shortcutFor, keyEvent, isTextTarget, escapeKeyEvents, stepDepth, SHORTCUTS } from '../../src/features/design/keymap.js';

const k = (key, extra = {}) => ({ key, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, code: '', ...extra });

test('the shortcut table', () => {
  assert.deepEqual(shortcutFor(k('c')), { action: 'mode', value: 'comment' });
  assert.deepEqual(shortcutFor(k('I')), { action: 'mode', value: 'interact' });
  assert.deepEqual(shortcutFor(k('r')), { action: 'mode', value: 'region' });
  assert.deepEqual(shortcutFor(k('s')), { action: 'mode', value: 'sketch' });
  assert.deepEqual(shortcutFor(k('[')), { action: 'wider' });
  assert.deepEqual(shortcutFor(k(']')), { action: 'narrower' });
  assert.deepEqual(shortcutFor(k('j')), { action: 'next_note' });
  assert.deepEqual(shortcutFor(k('k')), { action: 'prev_note' });
  assert.deepEqual(shortcutFor(k('2')), { action: 'preset', value: 'tablet' });
  assert.deepEqual(shortcutFor(k('0')), { action: 'fit_toggle' });
  assert.deepEqual(shortcutFor(k('R', { shiftKey: true })), { action: 'reload' });
  assert.deepEqual(shortcutFor(k('?', { shiftKey: true })), { action: 'help' });
  assert.equal(shortcutFor(k('x')), null);
});

test('shortcuts stay quiet in text fields, while capturing and with modifiers; Esc always acts', () => {
  assert.equal(shortcutFor(k('c'), { textFocus: true }), null);
  assert.equal(shortcutFor(k('c'), { capturing: true }), null);
  assert.equal(shortcutFor(k('c', { ctrlKey: true })), null);
  assert.deepEqual(shortcutFor(k('Escape'), { capturing: true, textFocus: true }), { action: 'escape' });
  assert.equal(isTextTarget({ tagName: 'TEXTAREA' }), true);
  assert.equal(isTextTarget({ tagName: 'BUTTON' }), false);
  assert.equal(isTextTarget({ tagName: 'DIV', isContentEditable: true }), true);
});

test('keys forwarded to the app: printable text, modifiers, never Escape', () => {
  assert.deepEqual(keyEvent('down', k('a', { code: 'KeyA' })), { type: 'key', action: 'down', key: 'a', code: 'KeyA', text: 'a', modifiers: 0 });
  assert.equal(keyEvent('up', k('a', { code: 'KeyA' })).text, null);
  assert.equal(keyEvent('down', k('Enter', { code: 'Enter' })).text, null);
  assert.equal(keyEvent('down', k('a', { ctrlKey: true })).text, null);
  assert.equal(keyEvent('down', k('a', { ctrlKey: true })).modifiers, 2);
  assert.equal(keyEvent('down', k('Escape')), null);
  assert.deepEqual(escapeKeyEvents().map((e) => [e.key, e.action]), [['Escape', 'down'], ['Escape', 'up']]);
});

test('Wider ([) adds one level, Narrower (]) removes one and stops at the picked element', () => {
  assert.equal(stepDepth(0, 1), 1);
  assert.equal(stepDepth(1, 1), 2);
  assert.equal(stepDepth(2, -1), 1);
  assert.equal(stepDepth(0, -1), 0);
  assert.deepEqual(SHORTCUTS.find(([key]) => key === '[ / ]'), ['[ / ]', 'Wider / narrower selection']);
});
