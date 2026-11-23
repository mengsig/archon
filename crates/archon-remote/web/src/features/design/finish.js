// Finish: the blocked text, the cancel-queued confirmation, the review cards,
// Confirm with its stale and conflict handling, and the ledger's gate answer.
import { h } from '../../ui/dom.js';
import { openSheet, confirmSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { queuedNotes, imageCapWarning, ledgerLede, afterAssetOf, noVisibleChange } from './state.js';
import { reconcile } from '../../ui/list.js';
import { statusChip } from './notes.js';
import { IMAGE_CAP_WARN_MARGIN, designErrorText } from '../../core/design.js';

// The daemon's answer when Confirm would approve no note (the choices made
// here left nothing in force): the review asks to approve the design as it is.
export const ASKS_NO_CHANGE = /approve the design as it is/i;
export const NO_CHANGE_QUESTION = 'Nothing new to approve in this session. Approve the design as it is?';
const LEDE = 'Review what the agent will keep. Each picture is checked against the final code.';

export function finishBlockedText(state) {
  if (state.finishBlocker) return state.finishBlocker;
  return null;
}

// "Cancel 2 queued notes and finish?" when notes are still waiting.
export async function startFinish(ctx, state, { onDone, onOpen, onClose }) {
  const queued = queuedNotes(state.notes);
  let cancelQueued = false;
  if (queued.length) {
    const n = queued.length;
    const ok = await confirmSheet({ title: 'Finish now?', message: `Cancel ${n} queued note${n === 1 ? '' : 's'} and finish?`, confirmLabel: 'Cancel notes and finish', cancelLabel: 'Keep editing' });
    if (!ok) return;
    cancelQueued = true;
  }
  try {
    const finish = await ctx.design.finishCapture(state.session.id, { cancelQueued });
    openReview(ctx, state, finish, { onDone, onOpen, onClose });
  } catch (err) {
    toast(designErrorText(err), { tone: 'red' });
    onDone({ reload: true });
  }
}

// A session already finishing: design_finish_capture without recapture hands back
// the existing finish view, so the review sheet can be reopened after it was closed.
export async function resumeFinish(ctx, state, { onDone, onOpen, onClose }) {
  try {
    const finish = await ctx.design.finishCapture(state.session.id, { recapture: false });
    openReview(ctx, state, finish, { onDone, onOpen, onClose });
  } catch (err) {
    toast(designErrorText(err), { tone: 'red' });
    onDone({ reload: true });
  }
}

export function openReview(ctx, state, initial, { onDone, onOpen, onClose }) {
  let finish = initial;
  let message = '';
  let askNoChange = false;
  const session = state.session.id;
  // Only what the operator switched by hand: a card that had no picture when
  // the review opened must not stay "left out" once a Capture gives it one.
  const chosen = new Map();
  const included = (card) => (chosen.has(card.note) ? chosen.get(card.note) : card.include);
  const superseded = new Map();
  const seed = () => {
    for (const c of finish.cards) {
      if (!superseded.has(c.note)) superseded.set(c.note, c.superseded_by ?? '');
    }
  };
  seed();
  const body = h('div', { class: 'finish' });
  let sheet = null;
  let busy = false;

  async function run(fn) {
    if (busy) return;
    busy = true;
    draw();
    try { await fn(); } finally { busy = false; if (!sheet?.closed) draw(); }
  }

  async function recapture(text) {
    finish = await ctx.design.finishCapture(session, { recapture: true });
    chosen.clear(); superseded.clear(); seed();
    askNoChange = false;
    message = text;
  }

  async function confirm() {
    try {
      await ctx.design.finishConfirm(session, {
        capture: finish.capture,
        include: Object.fromEntries(chosen),
        superseded: Object.fromEntries([...superseded].map(([k, v]) => [k, v ? v.trim() : null])),
        confirmNoChange: finish.no_change_only || askNoChange,
      });
      sheet.close();
      toast('Finishing: the agent is wrapping up.', { tone: 'green' });
      onDone({ reload: true });
    } catch (err) {
      if (err.code === 'stale') {
        try { await recapture('The code changed while you were reviewing, so Archon captured the pictures again. Review them and confirm.'); } catch (e2) { message = designErrorText(e2); }
      } else if (err.code === 'conflict') {
        message = designErrorText(err);
        sheet.close();
        toast(`${designErrorText(err)} Reloading.`, { tone: 'amber' });
        onDone({ reload: true });
      } else if (!askNoChange && ASKS_NO_CHANGE.test(err?.message ?? '')) {
        askNoChange = true;
        message = '';
      } else message = designErrorText(err);
    }
  }

  async function back() {
    try { await ctx.design.finishCancel(session); } catch (err) { toast(designErrorText(err), { tone: 'red' }); }
    sheet.close();
    onDone({ reload: true });
  }

  async function capture(card) {
    try {
      const out = await ctx.design.finalCapture(session, finish.capture, card.note);
      if (out?.cards) { finish = out; seed(); } else finish = await ctx.design.finishCapture(session, {});
      askNoChange = false;
      message = '';
    } catch (err) { message = designErrorText(err); }
  }

  function cardEl(card) {
    const options = finish.cards.filter((c) => c.note !== card.note).map((c) => h('option', { value: c.note }));
    const list = h('datalist', { id: `sup-${card.note}` }, options);
    const sup = h('input', { class: 'input input-inline', type: 'text', list: `sup-${card.note}`, value: superseded.get(card.note) ?? '', placeholder: 'e.g. D5', 'aria-label': `${card.note} superseded by`, autocomplete: 'off' });
    sup.addEventListener('input', () => superseded.set(card.note, sup.value));
    // A changed choice may put a note back in force: ask Confirm again.
    // Relabelled in place: this fires on blur, and redrawing then would
    // replace the button the operator is clicking.
    sup.addEventListener('change', () => { if (askNoChange) { askNoChange = false; relabel(); } });
    const inc = h('input', { class: 'switch-input', type: 'checkbox', role: 'switch', checked: included(card) !== false, 'aria-label': `Include picture for ${card.note}`, disabled: !card.data_url || card.earlier });
    inc.addEventListener('change', () => { chosen.set(card.note, inc.checked); askNoChange = false; draw(); });
    return h('li', { class: 'finish-card' },
      h('div', { class: 'finish-card-head' }, h('strong', { text: card.note }), card.earlier ? h('span', { class: 'note-tag', text: 'Earlier session' }) : null, card.superseded_by ? h('span', { class: 'note-tag', text: `Superseded by ${card.superseded_by}` }) : null),
      h('p', { class: 'note-text', text: card.text }),
      card.data_url ? h('img', { class: 'finish-img', src: card.data_url, alt: `Final picture for ${card.note}` }) : null,
      card.caption ? h('p', { class: 'cmp-caption', text: card.caption }) : null,
      card.not_found && !card.earlier ? h('div', { class: 'callout callout-amber', role: 'note' },
        h('p', { class: 'callout-text', text: 'Not found on a fresh load: open it in the preview and press Capture.' }),
        h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => run(() => capture(card)) }, 'Capture')) : null,
      card.left_history ? h('p', { class: 'note-hint', text: 'This note’s change is no longer in the code history, so there is no picture to include.' }) : null,
      h('label', { class: 'switch-row' }, h('span', { class: 'switch-text' }, h('span', { class: 'row-label', text: 'Include picture' })), h('span', { class: 'switch' }, inc, h('span', { class: 'switch-track', 'aria-hidden': 'true' }))),
      h('label', { class: 'field-label', text: 'Superseded by' }, sup), list);
  }

  let lede = null;
  let confirmButton = null;
  function relabel() {
    const noChange = finish.no_change_only || askNoChange;
    if (lede) {
      lede.className = noChange ? 'finish-question' : 'finish-lede';
      lede.textContent = noChange ? NO_CHANGE_QUESTION : LEDE;
    }
    if (confirmButton) confirmButton.textContent = noChange ? 'Approve as it is' : 'Confirm';
  }

  function draw() {
    const picked = finish.cards.filter((c) => c.data_url && included(c)).length;
    const warn = imageCapWarning(state.imagesUsed, state.imagesMax, picked, IMAGE_CAP_WARN_MARGIN);
    const noChange = finish.no_change_only || askNoChange;
    body.replaceChildren(...[
      message ? h('p', { class: 'callout callout-amber', role: 'status', text: message }) : null,
      (lede = h('p', { class: noChange ? 'finish-question' : 'finish-lede', text: noChange ? NO_CHANGE_QUESTION : LEDE })),
      warn ? h('p', { class: 'callout callout-amber', role: 'note', text: warn }) : null,
      h('ul', { class: 'finish-cards' }, finish.cards.map(cardEl)),
      h('div', { class: 'sheet-actions' },
        h('button', { class: 'button button-plain', type: 'button', disabled: busy, onclick: () => run(back) }, 'Back to editing'),
        (confirmButton = h('button', { class: 'button button-primary', type: 'button', disabled: busy, onclick: () => run(confirm) }, noChange ? 'Approve as it is' : 'Confirm')))].filter(Boolean));
  }
  draw();
  onOpen?.();
  sheet = openSheet({ title: 'Review before finishing', content: body, size: 'tall', onClose: () => onClose?.() });
}

// After Confirm: the ledger link and the gate's three answers, through the same `answer` request the inbox uses.
export function gateChoices(question) {
  const options = question?.options ?? [];
  const pick = (re, fallback) => options.find((o) => re.test(o)) ?? fallback;
  return [
    { id: 'approve', label: 'Approve', send: () => pick(/approv/i, 'approve') },
    { id: 'revise', label: 'Ask for changes', send: (text) => (text ? `${pick(/revis|change/i, 'revise')}: ${text}` : pick(/revis|change/i, 'revise')), needsText: true },
    { id: 'reject', label: 'Reject', send: () => pick(/reject/i, 'reject') },
  ];
}

export function gateQuestionFor(state, item) {
  return (state.questions ?? []).find((q) => q.item === item && !q.answer) ?? null;
}

// The session's notes with their pictures, and the gate's three answers once the operator confirmed.
export function ledgerNotes(notes) {
  return notes.filter((n) => !n.read_only && n.kind !== 'revert').sort((a, b) => a.number - b.number);
}

// `onAnswered` runs when the gate's question is answered here or elsewhere,
// so the screen asks for the session's end (and its decision) at once.
export function ledgerPanel(ctx, item, getState = () => null, onAnswered = () => {}) {
  const box = h('section', { class: 'design-ledger', 'aria-label': 'Ledger and approval', hidden: true });
  const title = h('h2', { class: 'ledger-title' });
  const lede = h('p', { class: 'ledger-lede' });
  const rows = h('ul', { class: 'ledger-list', 'aria-label': 'Design ledger' });
  const error = h('p', { class: 'form-error', role: 'alert' });
  const text = h('input', { class: 'input input-inline', type: 'text', placeholder: 'What should change?', 'aria-label': 'What to change', autocomplete: 'off' });
  const buttons = h('div', { class: 'ledger-actions' });
  const gate = h('div', { class: 'ledger-gate' }, buttons, text, error);
  const link = h('a', { class: 'link-button', href: `#/item/${item}`, text: 'Open the item' });
  box.append(title, lede, rows, gate, link);
  const pictures = new Map();
  let busy = false;
  let shownGate = null;
  let seenQuestion = null; // the open question last seen, to notice it being answered

  const picture = (id, label) => {
    const fig = h('figure', { class: 'ledger-pic' }, h('div', { class: 'ledger-pic-img' }, h('span', { class: 'muted', text: 'Loading…' })), h('figcaption', { class: 'cmp-caption', text: label }));
    if (!pictures.has(id)) pictures.set(id, ctx.design.asset(item, id));
    pictures.get(id).then((asset) => fig.querySelector('.ledger-pic-img').replaceChildren(h('img', { class: 'ledger-img', src: asset.data_url, alt: `${label} picture` })))
      .catch((err) => { pictures.delete(id); fig.querySelector('.ledger-pic-img').replaceChildren(h('span', { class: 'form-error', text: designErrorText(err) })); });
    return fig;
  };
  const row = (note) => h('li', { class: 'ledger-row' },
    h('div', { class: 'note-head' }, h('strong', { class: 'note-id', text: note.id }), statusChip(note)),
    h('p', { class: 'note-text', text: note.text }),
    note.before_asset || afterAssetOf(note) ? h('div', { class: 'ledger-pics' },
      note.before_asset ? picture(note.before_asset, 'Before') : null,
      afterAssetOf(note) ? picture(afterAssetOf(note), 'After') : null) : null,
    noVisibleChange(note) !== null ? h('p', { class: 'note-hint ledger-unchanged', role: 'status', text: `No visible change captured. ${noVisibleChange(note)}` }) : null);

  const answer = async (choice, q) => {
    if (busy) return;
    if (choice.needsText && !text.value.trim()) { error.textContent = 'Say what should change first.'; text.focus(); return; }
    busy = true; error.textContent = '';
    for (const b of buttons.children) b.disabled = true;
    try { await ctx.cmds.answer(q.id, choice.send(text.value.trim())); text.value = ''; ctx.refresh.one('questions'); toast('Answer sent', { tone: 'green' }); onAnswered(); }
    catch (err) { error.textContent = designErrorText(err); }
    finally { busy = false; for (const b of buttons.children) b.disabled = false; }
  };
  const drawGate = (q) => {
    const key = q ? q.id : null;
    if (key === shownGate) return;
    shownGate = key;
    buttons.replaceChildren(...(q ? gateChoices(q).map((c) => h('button', { class: ['button', c.id === 'approve' ? 'button-primary' : 'button-plain'], type: 'button', 'data-choice': c.id, onclick: () => answer(c, q) }, c.label)) : []));
  };

  const update = () => {
    const state = getState();
    const s = state?.session?.state;
    const confirmed = s === 'confirmed' && !state.ended;
    const show = Boolean(state?.session) && confirmed;
    box.hidden = !show;
    if (!show) return;
    const q = ctx.canOperate && confirmed ? gateQuestionFor(ctx.store.get(), item) : null;
    // Answered from the inbox, chat or another device: the session ends with the decision.
    if (seenQuestion && !q) onAnswered();
    seenQuestion = q ? q.id : null;
    title.textContent = 'Waiting for your approval';
    lede.textContent = ledgerLede(q, ctx.canOperate);
    drawGate(q);
    gate.hidden = !q;
    link.hidden = false;
    reconcile(rows, ledgerNotes(state.notes), { key: (n) => n.id, signature: (n) => JSON.stringify([n.status, n.text, n.undone_by, n.before_asset, n.after_asset, n.final_asset, n.final_included]), render: row });
    if (!rows.children.length) rows.replaceChildren(h('li', { class: 'ledger-empty muted', text: 'No notes were made in this session.' }));
    else rows.querySelector('.ledger-empty')?.remove();
  };
  ctx.refresh.one('questions');
  const off = ctx.store.watch((s) => s.questions, update);
  return { element: box, update, destroy: off };
}
