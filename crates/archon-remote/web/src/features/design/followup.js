// References and follow-ups: the "+ Reference a change" picker with its chips,
// @D autocomplete in a text box, and the follow-up box under a chain.
import { h } from '../../ui/dom.js';
import { MAX_NOTE_CHARS, designErrorText } from '../../core/design.js';
import { mentionAt, suggestRefs, applyMention, addRef, pickableRefs } from './chains.js';
import { sendsOnEnter } from './composer.js';

const words = (n) => n.text || (n.kind === 'page' ? 'Note on the whole page' : n.id);

// The chosen references (chips) and the list to choose them from.
export function createRefPicker({ getNotes, thumbs, onChange = () => {} }) {
  let refs = [];
  const chips = h('span', { class: 'ref-chips' });
  const list = h('ul', { class: 'ref-list', 'aria-label': 'Earlier notes to reference', hidden: true });
  const error = h('p', { class: 'form-error ref-error', role: 'alert' });
  const toggle = h('button', { class: 'button button-plain button-small', type: 'button', 'aria-expanded': 'false', onclick: () => (list.hidden ? open() : close()) }, '+ Reference a change');
  const row = h('div', { class: 'ref-row' }, toggle, chips);

  function draw() {
    chips.replaceChildren(...refs.map((id) => h('button', { class: 'ref-chip', type: 'button', 'aria-label': `Remove reference ${id}`, onclick: () => { refs = refs.filter((r) => r !== id); error.textContent = ''; draw(); onChange(refs); } }, `${id} ×`)));
  }
  function close() { list.hidden = true; toggle.setAttribute('aria-expanded', 'false'); }
  function open() {
    const options = pickableRefs(getNotes(), refs);
    list.replaceChildren(...(options.length
      ? options.map((n) => h('li', {}, h('button', { class: 'ref-option', type: 'button', onclick: () => { if (add(n.id)) close(); } }, thumbs.element(n, 's'), h('strong', { class: 'ref-id', text: n.id }), h('span', { class: 'ref-words', text: words(n) }))))
      : [h('li', { class: 'list-status', text: 'No earlier notes to reference.' })]));
    list.hidden = false;
    toggle.setAttribute('aria-expanded', 'true');
  }
  function add(id) {
    const out = addRef(refs, id);
    error.textContent = out.error;
    if (out.error) return false;
    if (out.refs !== refs) { refs = out.refs; draw(); onChange(refs); }
    return true;
  }
  return {
    row, list, error, add, close,
    get refs() { return refs; },
    clear() { refs = []; error.textContent = ''; close(); draw(); onChange(refs); },
  };
}

// The "@D" list under a text box, with keyboard choice. `onPick(id)` returns false when the reference is refused.
export function attachMentions(textarea, { getNotes, picker }) {
  const box = h('ul', { class: 'ref-list mention-list', role: 'listbox', 'aria-label': 'Notes matching what you typed', hidden: true });
  let mention = null;
  let options = [];
  let active = 0;
  const hide = () => { box.hidden = true; mention = null; };
  const paint = () => box.replaceChildren(...options.map((n, i) => h('li', {}, h('button', {
    class: ['ref-option', i === active && 'is-active'], type: 'button', role: 'option', 'aria-selected': String(i === active),
    onmousedown: (e) => e.preventDefault(), onclick: () => pick(n.id),
  }, h('strong', { class: 'ref-id', text: n.id }), h('span', { class: 'ref-words', text: words(n) })))));
  function refresh() {
    mention = mentionAt(textarea.value, textarea.selectionStart ?? textarea.value.length);
    options = mention ? suggestRefs(getNotes(), mention.query, picker.refs) : [];
    if (!options.length) return hide();
    active = Math.min(active, options.length - 1);
    paint();
    box.hidden = false;
  }
  function pick(id) {
    const at = mention;
    hide();
    if (!at || !picker.add(id)) return;
    const out = applyMention(textarea.value, at, id);
    textarea.value = out.text;
    textarea.setSelectionRange(out.caret, out.caret);
    textarea.focus();
  }
  textarea.addEventListener('input', refresh);
  textarea.addEventListener('click', refresh);
  textarea.addEventListener('blur', () => setTimeout(() => { if (document.activeElement !== textarea) hide(); }, 150));
  // Captures first: with the list open, Enter chooses instead of sending.
  textarea.addEventListener('keydown', (e) => {
    if (box.hidden) return;
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') { e.preventDefault(); active = (active + (e.key === 'ArrowDown' ? 1 : options.length - 1)) % options.length; paint(); }
    else if (e.key === 'Enter' || e.key === 'Tab') { e.preventDefault(); e.stopImmediatePropagation(); pick(options[active].id); }
    else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); hide(); }
  }, true);
  return { element: box, hide };
}

// A follow-up box for one chain. `onSend({target, text, refs})` rejects with the refusal.
export function createFollowBox({ getNotes, thumbs, onSend }) {
  let target = null;
  let busy = false;
  const label = h('p', { class: 'follow-label' });
  const hint = h('p', { class: 'note-hint follow-hint', hidden: true });
  const text = h('textarea', { class: 'input textarea follow-text', rows: '2', maxlength: String(MAX_NOTE_CHARS), placeholder: 'Say what to change next. Type @D to refer to an earlier note.' });
  const picker = createRefPicker({ getNotes, thumbs });
  const mentions = attachMentions(text, { getNotes, picker });
  const send = h('button', { class: 'button button-primary button-small', type: 'submit' }, 'Send follow-up');
  const error = h('p', { class: 'form-error', role: 'alert' });
  const el = h('form', { class: 'follow-box', novalidate: true }, label, text, mentions.element, picker.list, h('div', { class: 'follow-actions' }, picker.row, send), picker.error, hint, error);
  const submit = async () => {
    if (busy) return;
    const body = text.value.trim();
    if (!body) { error.textContent = 'Write what should change first.'; text.focus(); return; }
    error.textContent = '';
    busy = true; send.disabled = true;
    try {
      const sent = await onSend({ target, text: body, refs: [...picker.refs] });
      if (sent !== false) { text.value = ''; picker.clear(); }
    } catch (err) {
      error.textContent = designErrorText(err);
    } finally { busy = false; send.disabled = false; }
  };
  el.addEventListener('submit', (e) => { e.preventDefault(); submit(); });
  text.addEventListener('keydown', (e) => { if (sendsOnEnter(e, text.value)) { e.preventDefault(); submit(); } });
  return {
    element: el,
    setTarget(id, labelText, hintText = '') {
      target = id;
      label.textContent = labelText;
      text.setAttribute('aria-label', `Follow-up on ${id}`);
      hint.textContent = hintText;
      hint.hidden = !hintText;
    },
    get target() { return target; },
    setDisabled(disabled) { for (const c of el.querySelectorAll('textarea, button, input')) c.disabled = disabled; },
    focus() { text.focus({ preventScroll: true }); },
    get text() { return text.value; },
  };
}
