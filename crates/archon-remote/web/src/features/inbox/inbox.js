// The inbox: questions and gates waiting on the operator, answered ones,
// then recent notifications from the feed.
import { h } from '../../ui/dom.js';
import { reconcile } from '../../ui/list.js';
import { screenHeader, section, chip, emptyState, errorNote, loadErrorNotes, skeletonRows } from '../../ui/components.js';
import { age } from '../../core/format.js';
import { questionCard } from './question-card.js';

const RECENT_ANSWERS = 15;
const NOTIFICATIONS = 40;
const KIND_LABEL = { decision: 'Decision', blocker: 'Blocker', outcome: 'Outcome', question: 'Question', report: 'Report', progress: 'Progress' };
const KIND_TONE = { decision: 'accent', blocker: 'red', outcome: 'green', question: 'amber', report: 'accent' };

export function mountInbox(view, ctx) {
  const open = h('ul', { class: 'question-list', 'aria-label': 'Open questions' });
  const openStatus = h('div', { class: 'list-status' });
  const answered = h('ul', { class: 'plain-list', 'aria-label': 'Answered questions' });
  const notes = h('ul', { class: 'plain-list', 'aria-label': 'Notifications' });
  const notesStatus = h('div', { class: 'list-status' });
  const answeredSection = section('Recent', answered);
  view.append(screenHeader({ title: 'Inbox' }),
    section('Needs you', openStatus, open), answeredSection, section('Notifications', notesStatus, notes));

  const renderQuestions = () => {
    const state = ctx.store.get();
    const titles = new Map((state.summaries ?? []).map((s) => [s.id, s.title]));
    const all = state.questions ?? [];
    const waiting = all.filter((q) => !q.answer).sort((a, b) => a.number - b.number);
    reconcile(open, waiting, {
      key: (q) => q.id, signature: (q) => q.text + titles.get(q.item),
      render: (q) => questionCard(ctx, q, titles.get(q.item)),
    });
    openStatus.replaceChildren(...loadErrorNotes(state.errors, ['summaries'], ctx.refresh.one));
    if (state.errors.questions) openStatus.append(errorNote(state.errors.questions, () => ctx.refresh.one('questions')));
    else if (state.questions === null) openStatus.append(skeletonRows(2));
    else if (waiting.length === 0) openStatus.append(emptyState({ title: 'All clear', text: 'Nothing is waiting on you.', iconName: 'check' }));
    const done = all.filter((q) => q.answer).sort((a, b) => Date.parse(b.answered_at) - Date.parse(a.answered_at)).slice(0, RECENT_ANSWERS);
    answeredSection.hidden = done.length === 0;
    reconcile(answered, done, { key: (q) => q.id, signature: (q) => q.answer, render: answeredRow });
  };
  const renderNotes = () => {
    const { feed, errors } = ctx.store.get();
    const entries = notificationsOf(feed);
    reconcile(notes, entries, { key: (f) => f.seq, signature: (f) => age(f.at), render: noteRow });
    // A failed feed load is not an empty one; it is retried while connected.
    notesStatus.replaceChildren(...(errors.feed ? [errorNote(errors.feed, ctx.resyncFeed)] : []));
    if (entries.length === 0) notes.replaceChildren(...(errors.feed ? [] : [h('li', { class: 'muted plain-empty', text: 'No notifications yet.' })]));
  };
  const offs = [
    ctx.store.watch((s) => [s.questions, s.summaries, s.errors.questions, s.errors.summaries], renderQuestions),
    ctx.store.watch((s) => [s.feed, s.errors.feed], renderNotes),
  ];
  return () => offs.forEach((off) => off());
}

function notificationsOf(feed) {
  const shown = [];
  for (let i = feed.length - 1; i >= 0 && shown.length < NOTIFICATIONS; i -= 1) {
    const kind = feed[i].entry?.entry;
    if ((kind === 'notification' && feed[i].entry.kind !== 'progress' && feed[i].entry.kind !== 'question') || kind === 'report') shown.push(feed[i]);
  }
  return shown;
}

function answeredRow(question) {
  return h('li', { class: 'plain-row' },
    h('p', { class: 'plain-row-title', text: `Q${question.number} · ${question.text}` }),
    h('p', { class: 'plain-row-text' }, h('span', { class: 'muted', text: 'Answered: ' }), question.answer),
    h('span', { class: 'plain-row-age', text: age(question.answered_at) }));
}

function noteRow(item) {
  const entry = item.entry;
  const kind = entry.entry === 'report' ? 'report' : entry.kind;
  const text = entry.entry === 'report' ? entry.title : entry.text;
  return h('li', { class: 'plain-row' },
    h('div', { class: 'plain-row-head' }, chip({ label: KIND_LABEL[kind] ?? kind, tone: KIND_TONE[kind] ?? 'neutral' }),
      h('span', { class: 'plain-row-age', text: age(item.at) })),
    h('p', { class: 'plain-row-text', text }),
    entry.item ? h('a', { class: 'link-button', href: `#/item/${entry.item}` }, 'Open item') : null);
}
