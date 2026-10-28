// One queue item as a tappable row: title and state, where it runs, and
// what it waits on.
import { h } from '../../ui/dom.js';
import { chip } from '../../ui/components.js';
import { stateLook, stateKind, age, usd, priorityLabel, isTerminal, waitsOnOperator } from '../../core/format.js';

export function repoNames(repos) {
  return new Map((repos ?? []).map((repo) => [repo.id, repo.name]));
}

export function rowSignature(summary, names, now) {
  return JSON.stringify([summary.updated_at, summary.title, summary.state, summary.stage, summary.spent_usd,
    summary.waiting_on?.prompt, summary.waiting_reason?.message, names.get(summary.repo), age(summary.updated_at, now)]);
}

export function itemRow(summary, names, now) {
  const look = stateLook(summary);
  const meta = [names.get(summary.repo) ?? 'no repository', summary.switched_to ?? summary.pipeline, age(summary.updated_at, now)]
    .filter(Boolean).join(' · ');
  return h('li', { class: 'item-row-wrap' },
    h('a', { class: 'item-row', href: `#/item/${summary.id}`, 'aria-label': `${summary.title}, ${look.label}` },
      h('div', { class: 'item-row-main' },
        h('span', { class: 'item-row-title', text: summary.title }),
        h('span', { class: 'item-row-meta', text: meta }),
        detailLine(summary)),
      h('div', { class: 'item-row-side' },
        chip(look),
        summary.priority === 'high' || summary.priority === 'critical'
          ? h('span', { class: ['priority', `priority-${summary.priority}`], text: priorityLabel(summary.priority) }) : null)));
}

function detailLine(summary) {
  const parts = [];
  if (stateKind(summary) === 'running' && summary.stage) parts.push(`Stage ${summary.stage}`);
  if (summary.spent_usd > 0) parts.push(usd(summary.spent_usd));
  const wait = isTerminal(summary) ? null : (summary.waiting_on?.prompt ?? summary.waiting_reason?.message);
  const blocked = summary.state?.state === 'blocked' ? summary.state.reason : null;
  const deps = summary.dependencies?.length ? `Depends on ${summary.dependencies.length} item${summary.dependencies.length > 1 ? 's' : ''}` : null;
  const note = blocked ?? wait ?? deps;
  if (!note && parts.length === 0) return null;
  return h('span', { class: 'item-row-detail' },
    parts.length ? h('span', { text: parts.join(' · ') }) : null,
    note ? h('span', { class: waitsOnOperator(summary) ? 'tone-text-amber' : 'muted', text: note }) : null);
}
