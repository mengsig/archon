// How each feed entry reads in the chat transcript.
import { h } from '../../ui/dom.js';
import { clockTime, safeHttpUrl } from '../../core/format.js';

const NOTE_LABEL = { decision: 'Decision', blocker: 'Blocker', outcome: 'Outcome', question: 'Question', progress: 'Progress' };

export function dayKey(iso) {
  const at = new Date(iso);
  return `${at.getFullYear()}-${at.getMonth()}-${at.getDate()}`;
}

export function transcriptEntry(item, note, questions) {
  const entry = item.entry;
  switch (entry.entry) {
    case 'operator': return bubble('operator', entry.text, item.at, operatorNote(note));
    case 'orchestrator': return bubble('orchestrator', entry.text, item.at);
    case 'question': return questionLine(entry, questions.get(entry.id));
    case 'answer': {
      const q = questions.get(entry.question);
      return line('answer', q ? `Answered Q${q.number}` : 'Answered', entry.text, item.at);
    }
    case 'notification': return line(`note-${entry.kind}`, NOTE_LABEL[entry.kind] ?? entry.kind, entry.text, item.at, itemLink(entry.item));
    case 'report': return reportLine(entry, item.at);
    case 'authority': return line('system', 'Authority', `${entry.action}: ${entry.result}`, item.at);
    case 'system': return line('system', 'System', entry.text, item.at);
    default: return line('system', entry.entry ?? 'Entry', JSON.stringify(entry).slice(0, 200), item.at);
  }
}

function bubble(who, text, at, extra = null) {
  return h('li', { class: ['bubble-row', `from-${who}`] },
    h('div', { class: 'bubble' }, h('p', { class: 'bubble-text', text })),
    h('div', { class: 'bubble-meta' }, h('span', { text: who === 'operator' ? `You · ${clockTime(at)}` : `Orchestrator · ${clockTime(at)}` }), extra));
}

function operatorNote(note) {
  if (!note || note.status === 'sent') return null;
  if (note.status === 'sending') return h('span', { class: 'bubble-note', text: 'Sending…' });
  return h('span', { class: 'bubble-note tone-text-amber', text: 'Saved. The orchestrator is offline and gets it once it is back.' });
}

function line(kind, label, text, at, extra = null) {
  return h('li', { class: ['feed-line', `feed-${kind}`] },
    h('p', { class: 'feed-line-text' }, h('span', { class: 'feed-line-label', text: `${label} · ${clockTime(at)}` }), h('span', { text })),
    extra);
}

function questionLine(entry, current) {
  const open = !(current?.answer ?? entry.answer);
  const link = open ? h('a', { class: 'link-button', href: '#/inbox' }, 'Answer in Inbox') : null;
  return line('question', `Q${entry.number}`, entry.text, entry.asked_at, link);
}

function reportLine(entry, at) {
  const url = safeHttpUrl(entry.location);
  const link = url ? h('a', { class: 'link-button', href: url, target: '_blank', rel: 'noopener noreferrer' }, 'Open report') : null;
  return line('report', 'Report', entry.title, at, link ?? itemLink(entry.item));
}

function itemLink(itemId) {
  return itemId ? h('a', { class: 'link-button', href: `#/item/${itemId}` }, 'Open item') : null;
}

export function pendingEntry(entry, { retry, discard }) {
  const failed = entry.status === 'failed';
  const note = {
    sending: h('span', { class: 'bubble-note', text: 'Sending…' }),
    sent: null,
    offline: h('span', { class: 'bubble-note tone-text-amber', text: 'Saved. The orchestrator is offline and gets it once it is back.' }),
    failed: h('span', { class: 'bubble-note tone-text-red', role: 'alert', text: `Not sent: ${entry.error}` }),
  }[entry.status];
  return h('li', { class: ['bubble-row', 'from-operator', 'is-pending', failed && 'is-failed'] },
    h('div', { class: 'bubble' }, h('p', { class: 'bubble-text', text: entry.text })),
    h('div', { class: 'bubble-meta' }, note,
      failed ? h('button', { class: 'link-button', type: 'button', onclick: retry }, 'Retry') : null,
      failed ? h('button', { class: 'link-button', type: 'button', onclick: discard }, 'Discard') : null));
}
