// How each conversation entry reads in the chat transcript.
import { h } from '../../ui/dom.js';
import { renderMarkdown } from '../../ui/markdown.js';
import { clockTime, safeHttpUrl } from '../../core/format.js';
import { stripRelayMark } from '../../core/conversation.js';

export function dayKey(iso) {
  const at = new Date(iso);
  return `${at.getFullYear()}-${at.getMonth()}-${at.getDate()}`;
}

// One conversation entry exactly as archond sent it; `note` is the send outcome of an operator line.
export function conversationEntry(entry, note) {
  switch (entry.kind) {
    case 'operator': return bubble('operator', stripRelayMark(entry.text), entry.at, operatorNote(note));
    case 'assistant': return bubble('orchestrator', entry.text, entry.at);
    case 'post': return line('post', 'Post', entry.text, entry.at, linkButtons(entry.links));
    case 'question': return line('question', 'Question', entry.text, entry.at, h('a', { class: 'link-button', href: '#/inbox' }, 'Answer in Inbox'));
    default: return line('system', String(entry.kind ?? 'Entry'), entry.text ?? '', entry.at, null, { plain: true });
  }
}

function linkButtons(links) {
  const buttons = (links ?? []).map(safeHttpUrl).filter(Boolean)
    .map((url) => h('a', { class: 'link-button', href: url, target: '_blank', rel: 'noopener noreferrer' }, new URL(url).host));
  return buttons.length > 0 ? h('div', { class: 'feed-line-links' }, ...buttons) : null;
}

function bubble(who, text, at, extra = null) {
  return h('li', { class: ['bubble-row', `from-${who}`] },
    h('div', { class: 'bubble' }, renderMarkdown(text, { className: 'bubble-text' })),
    h('div', { class: 'bubble-meta' }, h('span', { text: who === 'operator' ? `You · ${clockTime(at)}` : `Orchestrator · ${clockTime(at)}` }), extra));
}

function operatorNote(note) {
  if (!note || note.status === 'sent') return null;
  if (note.status === 'sending') return h('span', { class: 'bubble-note', text: 'Sending…' });
  return h('span', { class: 'bubble-note tone-text-amber', text: 'Saved. The orchestrator is offline and gets it once it is back.' });
}

function line(kind, label, text, at, extra = null, { plain = false } = {}) {
  return h('li', { class: ['feed-line', `feed-${kind}`] },
    h('div', { class: 'feed-line-text' }, h('span', { class: 'feed-line-label', text: `${label} · ${clockTime(at)}` }),
      plain ? h('span', { text }) : renderMarkdown(text, { className: 'feed-line-body' })),
    extra);
}

export function pendingEntry(entry, { retry, discard }) {
  const failed = entry.status === 'failed';
  const note = {
    sending: h('span', { class: 'bubble-note', text: 'Sending…' }),
    sent: h('span', { class: 'bubble-note', text: 'Sent. It shows as read once the orchestrator picks it up.' }),
    offline: h('span', { class: 'bubble-note tone-text-amber', text: 'Saved. The orchestrator is offline and gets it once it is back.' }),
    failed: h('span', { class: 'bubble-note tone-text-red', role: 'alert', text: `Not sent: ${entry.error}` }),
  }[entry.status];
  return h('li', { class: ['bubble-row', 'from-operator', 'is-pending', failed && 'is-failed'] },
    h('div', { class: 'bubble' }, renderMarkdown(entry.text, { className: 'bubble-text' })),
    h('div', { class: 'bubble-meta' }, note,
      failed ? h('button', { class: 'link-button', type: 'button', onclick: retry }, 'Retry') : null,
      failed ? h('button', { class: 'link-button', type: 'button', onclick: discard }, 'Discard') : null));
}
