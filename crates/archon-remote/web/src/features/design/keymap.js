// Keys. The shortcut table of the design screen (outside text fields and
// keyboard capture) and the translation of a DOM key into a forwarded Key event.
import { modifiersOf } from './input.js';

const MODES = { i: 'interact', c: 'comment', r: 'region', s: 'sketch' };
const PRESETS = { 1: 'desktop', 2: 'tablet', 3: 'phone' };

export function isTextTarget(target) {
  if (!target || typeof target.tagName !== 'string') return false;
  const tag = target.tagName.toLowerCase();
  return tag === 'input' || tag === 'textarea' || tag === 'select' || target.isContentEditable === true;
}

// Returns {action, value?} or null. Escape is always an action; the rest only when focus is free.
export function shortcutFor(event, { capturing = false, textFocus = false } = {}) {
  if (event.key === 'Escape') return { action: 'escape' };
  if (capturing || textFocus) return null;
  if (event.ctrlKey || event.metaKey || event.altKey) return null;
  const key = event.key;
  if (key === '?') return { action: 'help' };
  if (key === 'R' && event.shiftKey) return { action: 'reload' };
  if (event.shiftKey && key.length === 1 && key !== '?') return null;
  if (MODES[key.toLowerCase()] && key.length === 1) return { action: 'mode', value: MODES[key.toLowerCase()] };
  if (key === '[') return { action: 'narrower' };
  if (key === ']') return { action: 'wider' };
  if (key === 'j' || key === 'J') return { action: 'next_note' };
  if (key === 'k' || key === 'K') return { action: 'prev_note' };
  if (PRESETS[key]) return { action: 'preset', value: PRESETS[key] };
  if (key === '0') return { action: 'fit_toggle' };
  return null;
}

export const SHORTCUTS = [
  ['I / C / R / S', 'Interact / Comment / Region / Sketch'],
  ['Esc', 'Cancel the selection or composer; stop typing into the app'],
  ['[ / ]', 'Narrower / wider selection'],
  ['Enter', 'Send a one-line note'],
  ['Ctrl+Enter', 'Send a multi-line note'],
  ['J / K', 'Next / previous note'],
  ['1 / 2 / 3', 'Desktop / tablet / phone'],
  ['0', 'Fit / actual size'],
  ['Shift+R', 'Reload the preview'],
  ['?', 'This list'],
];

// A DOM key becomes the Key event the browser child gets. Escape never goes this way.
export function keyEvent(action, event) {
  if (event.key === 'Escape') return null;
  const printable = event.key.length === 1 && !event.ctrlKey && !event.metaKey;
  return { type: 'Key', action, key: event.key, code: event.code ?? '', text: printable && action === 'down' ? event.key : null, modifiers: modifiersOf(event) };
}

// The explicit "Send Esc to app" button.
export function escapeKeyEvents() {
  return ['down', 'up'].map((action) => ({ type: 'Key', action, key: 'Escape', code: 'Escape', text: null, modifiers: 0 }));
}
