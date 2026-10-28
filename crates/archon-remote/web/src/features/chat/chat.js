// Chat with the orchestrator, like `archon chat`: the replayed feed, live
// lines, and a composer that sends optimistically.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { reconcile } from '../../ui/list.js';
import { screenHeader, errorNote, loadErrorNotes, describeError } from '../../ui/components.js';
import { isQuiet } from '../../core/feed.js';
import { addPending, settlePending, removePending } from './outbox.js';
import { transcriptEntry, pendingEntry, dayKey } from './entries.js';

const NEAR_BOTTOM_PX = 120;
const MAX_LINE_CHARS = 16_000;

export function mountChat(view, ctx) {
  const subtitle = h('p', { class: 'chat-presence', role: 'status' });
  const transcript = h('ol', { class: 'transcript', role: 'log', 'aria-label': 'Conversation', 'aria-live': 'polite' });
  const status = h('div', { class: 'list-status' });
  const composer = buildComposer((text) => send(ctx, text));
  view.append(screenHeader({ title: 'Chat' }), subtitle, status, transcript, composer.form);

  const render = () => {
    const nearBottom = window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - NEAR_BOTTOM_PX;
    renderTranscript(ctx, transcript);
    const { errors } = ctx.store.get();
    status.replaceChildren(...(errors.feed ? [errorNote(errors.feed)] : []),
      ...loadErrorNotes(errors, ['questions', 'status'], ctx.refresh.one));
    if (nearBottom) scrollToEnd();
  };
  const offs = [
    ctx.store.watch((s) => [s.feed, s.outbox, s.chatNotes, s.questions, s.errors.feed, s.errors.questions, s.errors.status], render),
    ctx.store.watch((s) => [s.status, s.connection.state], () => renderPresence(subtitle, ctx.store.get())),
    trackKeyboard(),
  ];
  requestAnimationFrame(scrollToEnd);
  return () => offs.forEach((off) => off());
}

function scrollToEnd() {
  window.scrollTo({ top: document.documentElement.scrollHeight, behavior: 'instant' });
}

function renderTranscript(ctx, transcript) {
  const { feed, outbox, chatNotes, questions } = ctx.store.get();
  const numbers = new Map((questions ?? []).map((q) => [q.id, q]));
  const rows = [];
  let lastDay = null;
  for (const item of feed) {
    if (isQuiet(item)) continue;
    const day = dayKey(item.at);
    if (day !== lastDay) rows.push({ kind: 'day', key: `d-${day}`, at: item.at });
    lastDay = day;
    rows.push({ kind: 'feed', key: `f-${item.seq}`, item, note: chatNotes[item.seq] ?? null });
  }
  for (const entry of outbox) rows.push({ kind: 'pending', key: `p-${entry.localId}`, entry });
  reconcile(transcript, rows, {
    key: (row) => row.key,
    signature: (row) => JSON.stringify([row.note, row.entry, row.item?.entry?.entry === 'question' ? numbers.get(row.item.entry.id)?.answer ?? null : null]),
    render: (row) => {
      if (row.kind === 'pending') return pendingEntry(row.entry, { retry: () => resend(ctx, row.entry), discard: () => removePending(ctx.store, row.entry.localId) });
      if (row.kind === 'day') return h('li', { class: 'day-divider', text: new Date(row.at).toLocaleDateString(undefined, { weekday: 'long', month: 'short', day: 'numeric' }) });
      return transcriptEntry(row.item, row.note, numbers);
    },
  });
  if (rows.length === 0) transcript.replaceChildren(h('li', { class: 'transcript-empty', text: 'Say hello. The orchestrator reads every line you send here.' }));
}

function renderPresence(subtitle, state) {
  const status = state.status;
  let text = 'Checking the orchestrator…';
  let tone = 'neutral';
  if (state.connection.state !== 'open') {
    text = 'Not connected to the host';
    tone = 'amber';
  } else if (status?.aether?.state === 'disconnected') {
    text = 'Orchestrator offline · lines wait for it';
    tone = 'amber';
  } else if (status) {
    text = status.orchestrator ? `Orchestrator ${status.orchestrator.status}` : 'Orchestrator starts with your first line';
    tone = 'green';
  }
  subtitle.dataset.tone = tone;
  subtitle.replaceChildren(h('span', { class: 'presence-dot', 'aria-hidden': 'true' }), h('span', { text }));
}

function buildComposer(onSend) {
  const input = h('textarea', {
    class: 'composer-input', rows: '1', placeholder: 'Message the orchestrator', 'aria-label': 'Message',
    enterkeyhint: 'send', maxlength: String(MAX_LINE_CHARS),
  });
  const send = h('button', { class: 'composer-send', type: 'submit', 'aria-label': 'Send', disabled: true }, icon('send', { size: 20 }));
  const form = h('form', { class: 'composer', novalidate: true, 'aria-label': 'Send a message' }, input, send);
  const grow = () => {
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 140)}px`;
    send.disabled = input.value.trim() === '';
  };
  const submit = () => {
    const text = input.value.trim();
    if (!text) return;
    input.value = '';
    grow();
    onSend(text);
    input.focus();
  };
  input.addEventListener('input', grow);
  input.addEventListener('keydown', (event) => {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      submit();
    }
  });
  form.addEventListener('submit', (event) => {
    event.preventDefault();
    submit();
  });
  return { form, input };
}

async function send(ctx, text) {
  const localId = addPending(ctx.store, text);
  requestAnimationFrame(scrollToEnd);
  try {
    await ctx.cmds.say(text);
    settlePending(ctx.store, localId, 'sent');
  } catch (err) {
    // Unavailable means archond kept the line but could not hand it on yet.
    if (err.code === 'unavailable') settlePending(ctx.store, localId, 'offline', describeError(err));
    else settlePending(ctx.store, localId, 'failed', describeError(err));
  }
}

function resend(ctx, entry) {
  removePending(ctx.store, entry.localId);
  send(ctx, entry.text);
}

// Keeps the composer above an on-screen keyboard where the browser only
// shrinks the visual viewport (iOS Safari).
function trackKeyboard() {
  const vv = window.visualViewport;
  if (!vv) return () => {};
  const root = document.documentElement;
  const update = () => {
    const covered = Math.max(0, window.innerHeight - vv.height - vv.offsetTop);
    if (covered > 40) root.style.setProperty('--keyboard', `${Math.round(covered)}px`);
    else root.style.removeProperty('--keyboard');
    root.classList.toggle('keyboard-open', covered > 40);
  };
  vv.addEventListener('resize', update);
  vv.addEventListener('scroll', update);
  update();
  return () => {
    vv.removeEventListener('resize', update);
    vv.removeEventListener('scroll', update);
    root.style.removeProperty('--keyboard');
    root.classList.remove('keyboard-open');
  };
}
