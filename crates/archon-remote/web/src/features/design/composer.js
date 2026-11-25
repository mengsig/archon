// The composer: "What the agent will receive". The crop exactly as sent, a
// switch for it, the details as editable text with a switch, the operator's
// words, and Wider / Narrower for the selection.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { buildDetails, capText } from './details.js';
import { MAX_NOTE_CHARS, MAX_DETAILS_CHARS } from '../../core/design.js';
import { createRefPicker, attachMentions } from './followup.js';

function switchRow(label, checked, onChange) {
  const input = h('input', { class: 'switch-input', type: 'checkbox', role: 'switch', checked, 'aria-label': label });
  input.addEventListener('change', () => onChange(input.checked));
  return { input, row: h('label', { class: 'switch-row' }, h('span', { class: 'switch-text' }, h('span', { class: 'row-label', text: label })),
    h('span', { class: 'switch' }, input, h('span', { class: 'switch-track', 'aria-hidden': 'true' }))) };
}

// Enter sends a one-line note; once the note has a line break, Ctrl+Enter sends it.
export function sendsOnEnter(event, text) {
  if (event.key !== 'Enter' || event.isComposing) return false;
  if (event.ctrlKey || event.metaKey) return true;
  return !event.shiftKey && !text.includes('\n');
}

// resolved: DesignResolvedView {target, label, asset, data_url} or null for a note on the whole page.
export function createComposer({ onSend, onCancel, onDepth, canWiden = true, getNotes = () => [], thumbs = null }) {
  let resolved = null;
  let busy = false;
  const view = { sendPicture: true, sendDetails: true };
  const crop = h('img', { class: 'dcomposer-crop', alt: 'The picture the agent will receive' });
  const label = h('p', { class: 'dcomposer-target' });
  const picture = switchRow('Send picture', true, (v) => { view.sendPicture = v; crop.classList.toggle('is-off', !v); });
  const details = h('textarea', { class: 'input textarea dcomposer-details', rows: '5', 'aria-label': 'Details the agent will receive', maxlength: String(MAX_DETAILS_CHARS), spellcheck: 'false' });
  const detailsSwitch = switchRow('Send details', true, (v) => { view.sendDetails = v; details.disabled = !v; });
  const text = h('textarea', { class: 'input textarea dcomposer-text', rows: '2', 'aria-label': 'What should change', placeholder: 'What should change? Say it in plain words.', maxlength: String(MAX_NOTE_CHARS), enterkeyhint: 'send' });
  const error = h('p', { class: 'form-error', role: 'alert' });
  // References to earlier notes; shown only when the host offers design iterations.
  const picker = createRefPicker({ getNotes, thumbs: thumbs ?? { element: () => h('span') } });
  const mentions = attachMentions(text, { getNotes, picker });
  const refsBlock = h('div', { class: 'dcomposer-refs', hidden: true }, mentions.element, picker.list, picker.row, picker.error);
  const wider = h('button', { class: 'button button-plain', type: 'button', 'aria-label': 'Wider selection ([)', onclick: () => step(1) }, icon('region', { size: 18 }), ' Wider');
  const narrower = h('button', { class: 'button button-plain', type: 'button', 'aria-label': 'Narrower selection (])', onclick: () => step(-1) }, icon('pointer', { size: 18 }), ' Narrower');
  const send = h('button', { class: 'button button-primary', type: 'submit' }, icon('send', { size: 18 }), ' Send');
  const cancel = h('button', { class: 'button button-plain', type: 'button', onclick: () => onCancel() }, 'Cancel');
  const targetBlock = h('div', { class: 'dcomposer-target-block' }, h('h3', { class: 'dcomposer-heading', text: 'What the agent will receive' }),
    crop, label, picture.row, details, detailsSwitch.row, h('div', { class: 'dcomposer-depth' }, wider, narrower));
  const el = h('form', { class: 'dcomposer', novalidate: true, 'aria-label': 'New note', hidden: true }, targetBlock, text, refsBlock, error, h('div', { class: 'dcomposer-actions' }, cancel, send));

  function step(by) {
    if (!resolved || resolved.target?.region) return;
    onDepth(by);
  }
  const submit = () => {
    if (busy) return;
    const words = text.value.trim();
    if (!words) { error.textContent = 'Write what should change first.'; text.focus(); return; }
    error.textContent = '';
    onSend({
      text: capText(words, MAX_NOTE_CHARS), asset: resolved?.asset ?? null, sendPicture: Boolean(resolved?.asset) && view.sendPicture,
      sendDetails: Boolean(resolved) && view.sendDetails, details: details.value, resolved, refs: [...picker.refs],
    });
  };
  el.addEventListener('submit', (e) => { e.preventDefault(); submit(); });
  text.addEventListener('keydown', (e) => { if (sendsOnEnter(e, text.value)) { e.preventDefault(); submit(); } });
  void canWiden;

  return {
    element: el,
    get open() { return !el.hidden; },
    // Show a resolved element, region or sketch; null opens a note on the whole page.
    show(next, { keepText = true } = {}) {
      resolved = next;
      el.hidden = false;
      targetBlock.hidden = !next;
      if (next) {
        crop.hidden = !next.data_url;
        if (next.data_url) crop.src = next.data_url;
        label.textContent = next.label ?? '';
        details.value = next.details ?? buildDetails(next.target);
        wider.hidden = narrower.hidden = Boolean(next.target?.region) || !next.target;
        picture.row.hidden = !next.asset;
        detailsSwitch.row.hidden = details.hidden = false;
      }
      if (!keepText) text.value = '';
      error.textContent = '';
      text.focus({ preventScroll: true });
    },
    hide() { el.hidden = true; resolved = null; },
    setBusy(on) { busy = on; send.disabled = on; send.setAttribute('aria-busy', String(on)); },
    get busy() { return busy; },
    get resolved() { return resolved; },
    setError(message) { error.textContent = message; },
    reset() { text.value = ''; picker.clear(); this.hide(); },
    setRefsEnabled(on) { refsBlock.hidden = !on; },
    get text() { return text.value; },
    set text(v) { text.value = v; },
    setDisabled(disabled, note = '') {
      for (const control of el.querySelectorAll('textarea, button, input')) control.disabled = disabled;
      error.textContent = disabled ? note : '';
    },
  };
}
