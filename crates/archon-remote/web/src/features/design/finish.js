// Finish: the blocked text, the cancel-queued confirmation, the review cards,
// Confirm with its stale and conflict handling, and the ledger's gate answer.
import { h } from '../../ui/dom.js';
import { openSheet, confirmSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { queuedNotes, imageCapWarning } from './state.js';
import { IMAGE_CAP_WARN_MARGIN, designErrorText } from '../../core/design.js';

export const NO_CHANGE_QUESTION = 'No note changed the code. Approve the design as it is?';

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
  const session = state.session.id;
  const include = new Map();
  const superseded = new Map();
  const seed = () => {
    for (const c of finish.cards) {
      if (!include.has(c.note)) include.set(c.note, c.include);
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
    include.clear(); superseded.clear(); seed();
    message = text;
  }

  async function confirm() {
    try {
      await ctx.design.finishConfirm(session, {
        capture: finish.capture,
        include: Object.fromEntries(include),
        superseded: Object.fromEntries([...superseded].map(([k, v]) => [k, v ? v.trim() : null])),
        confirmNoChange: finish.no_change_only,
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
      message = '';
    } catch (err) { message = designErrorText(err); }
  }

  function cardEl(card) {
    const options = finish.cards.filter((c) => c.note !== card.note).map((c) => h('option', { value: c.note }));
    const list = h('datalist', { id: `sup-${card.note}` }, options);
    const sup = h('input', { class: 'input input-inline', type: 'text', list: `sup-${card.note}`, value: superseded.get(card.note) ?? '', placeholder: 'e.g. D5', 'aria-label': `${card.note} superseded by`, autocomplete: 'off' });
    sup.addEventListener('input', () => superseded.set(card.note, sup.value));
    const inc = h('input', { class: 'switch-input', type: 'checkbox', role: 'switch', checked: include.get(card.note) !== false, 'aria-label': `Include picture for ${card.note}`, disabled: !card.data_url });
    inc.addEventListener('change', () => { include.set(card.note, inc.checked); draw(); });
    return h('li', { class: 'finish-card' },
      h('div', { class: 'finish-card-head' }, h('strong', { text: card.note }), card.superseded_by ? h('span', { class: 'note-tag', text: `Superseded by ${card.superseded_by}` }) : null),
      h('p', { class: 'note-text', text: card.text }),
      card.data_url ? h('img', { class: 'finish-img', src: card.data_url, alt: `Final picture for ${card.note}` }) : null,
      card.caption ? h('p', { class: 'cmp-caption', text: card.caption }) : null,
      card.not_found ? h('div', { class: 'callout callout-amber', role: 'note' },
        h('p', { class: 'callout-text', text: 'Not found on a fresh load: open it in the preview and press Capture.' }),
        h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => run(() => capture(card)) }, 'Capture')) : null,
      card.left_history ? h('p', { class: 'note-hint', text: 'This note’s change is no longer in the code history, so there is no picture to include.' }) : null,
      h('label', { class: 'switch-row' }, h('span', { class: 'switch-text' }, h('span', { class: 'row-label', text: 'Include picture' })), h('span', { class: 'switch' }, inc, h('span', { class: 'switch-track', 'aria-hidden': 'true' }))),
      h('label', { class: 'field-label', text: 'Superseded by' }, sup), list);
  }

  function draw() {
    const included = [...include].filter(([k, v]) => v && finish.cards.find((c) => c.note === k)?.data_url).length;
    const warn = imageCapWarning(state.imagesUsed, state.imagesMax, included, IMAGE_CAP_WARN_MARGIN);
    const noChange = finish.no_change_only;
    body.replaceChildren(
      message ? h('p', { class: 'callout callout-amber', role: 'status', text: message }) : null,
      noChange ? h('p', { class: 'finish-question', text: NO_CHANGE_QUESTION })
        : h('p', { class: 'finish-lede', text: 'Review what the agent will keep. Each picture is checked against the final code.' }),
      warn ? h('p', { class: 'callout callout-amber', role: 'note', text: warn }) : null,
      h('ul', { class: 'finish-cards' }, finish.cards.map(cardEl)),
      h('div', { class: 'sheet-actions' },
        h('button', { class: 'button button-plain', type: 'button', disabled: busy, onclick: () => run(back) }, 'Back to editing'),
        h('button', { class: 'button button-primary', type: 'button', disabled: busy, onclick: () => run(confirm) }, noChange ? 'Approve as it is' : 'Confirm')));
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

export function ledgerPanel(ctx, item) {
  const box = h('section', { class: 'design-ledger', 'aria-label': 'Approve the design' });
  const error = h('p', { class: 'form-error', role: 'alert' });
  const text = h('input', { class: 'input input-inline', type: 'text', placeholder: 'What should change?', 'aria-label': 'What to change', autocomplete: 'off' });
  const render = () => {
    const q = gateQuestionFor(ctx.store.get(), item);
    box.replaceChildren(h('a', { class: 'link-button', href: `#/item/${item}`, text: 'View the ledger' }));
    if (!q || !ctx.canOperate) return;
    const answer = async (choice) => {
      if (choice.needsText && !text.value.trim()) { error.textContent = 'Say what should change first.'; text.focus(); return; }
      try { await ctx.cmds.answer(q.id, choice.send(text.value.trim())); ctx.refresh.one('questions'); toast('Answer sent', { tone: 'green' }); }
      catch (err) { error.textContent = designErrorText(err); }
    };
    box.append(h('div', { class: 'ledger-actions' }, gateChoices(q).map((c) => h('button', { class: ['button', c.id === 'approve' ? 'button-primary' : 'button-plain'], type: 'button', onclick: () => answer(c) }, c.label))), text, error);
  };
  ctx.refresh.one('questions');
  const off = ctx.store.watch((s) => s.questions, render);
  return { element: box, destroy: off };
}
