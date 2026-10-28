// Bottom sheets: a modal dialog that slides up, traps focus, closes on
// Escape or a tap outside, and gives focus back to where it came from.
import { h } from './dom.js';
import { icon } from './icons.js';
import { settled, nextFrame } from './motion.js';

const FOCUSABLE = 'button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';
let sheetCount = 0;

export function openSheet({ title, content, onClose = () => {}, size = 'auto' }) {
  if (typeof title !== 'string' || !title) throw new TypeError('a sheet has a title');
  const root = document.getElementById('overlay');
  if (!root) throw new Error('the app shell has no overlay layer');
  const opener = document.activeElement;
  const titleId = `sheet-title-${++sheetCount}`;
  const body = h('div', { class: 'sheet-body' }, content);
  const panel = h('div', { class: ['sheet', `sheet-${size}`], role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': titleId },
    h('div', { class: 'sheet-grabber', 'aria-hidden': 'true' }),
    h('header', { class: 'sheet-header' },
      h('h2', { class: 'sheet-title', id: titleId, text: title }),
      h('button', { class: 'icon-button', type: 'button', 'aria-label': 'Close', onclick: () => close() }, icon('close', { size: 20 }))),
    body);
  const backdrop = h('div', { class: 'sheet-backdrop', onclick: () => close() });
  const layer = h('div', { class: 'sheet-layer' }, backdrop, panel);
  let closing = null;

  function onKey(event) {
    if (event.key === 'Escape') {
      event.stopPropagation();
      close();
    } else if (event.key === 'Tab') {
      trapFocus(panel, event);
    }
  }

  async function close() {
    if (closing) return closing;
    closing = (async () => {
      layer.classList.remove('is-open');
      layer.removeEventListener('keydown', onKey);
      await settled(panel);
      layer.remove();
      setShellInert(root.childElementCount > 0);
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus({ preventScroll: true });
      onClose();
    })();
    return closing;
  }

  layer.addEventListener('keydown', onKey);
  root.append(layer);
  setShellInert(true);
  nextFrame().then(() => {
    layer.classList.add('is-open');
    const first = body.querySelector('[autofocus]') ?? body.querySelector(FOCUSABLE) ?? panel.querySelector(FOCUSABLE);
    first?.focus({ preventScroll: true });
  });
  return { close, panel, body, get closed() { return closing !== null; } };
}

function trapFocus(panel, event) {
  const nodes = [...panel.querySelectorAll(FOCUSABLE)].filter((el) => el.offsetParent !== null);
  if (nodes.length === 0) return;
  const first = nodes[0];
  const last = nodes.at(-1);
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}

function setShellInert(inert) {
  const shell = document.getElementById('shell');
  if (shell) shell.inert = inert;
}

// A yes/no question in a sheet; resolves true only for the confirm button.
export function confirmSheet({ title, message, confirmLabel, tone = 'danger', cancelLabel = 'Keep it' }) {
  return new Promise((resolve) => {
    let answer = false;
    const confirm = h('button', { class: ['button', 'button-block', `button-${tone}`], type: 'button',
      onclick: () => { answer = true; sheet.close(); } }, confirmLabel);
    const cancel = h('button', { class: 'button button-block button-plain', type: 'button', onclick: () => sheet.close() }, cancelLabel);
    const sheet = openSheet({
      title,
      content: h('div', { class: 'confirm' }, h('p', { class: 'confirm-message', text: message }), confirm, cancel),
      onClose: () => resolve(answer),
    });
  });
}
