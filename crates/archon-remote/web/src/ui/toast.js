// Short, polite status messages above the tab bar.
import { h } from './dom.js';
import { settled } from './motion.js';

const SHOW_MS = 3200;

export function toast(message, { tone = 'neutral' } = {}) {
  const root = document.getElementById('toasts');
  if (!root) throw new Error('the app shell has no toast region');
  const note = h('div', { class: ['toast', `toast-${tone}`], role: tone === 'red' ? 'alert' : 'status', text: message });
  root.append(note);
  requestAnimationFrame(() => note.classList.add('is-shown'));
  setTimeout(async () => {
    note.classList.remove('is-shown');
    await settled(note);
    note.remove();
  }, SHOW_MS);
}
