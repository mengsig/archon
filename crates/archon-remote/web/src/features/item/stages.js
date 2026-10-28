// An item's stage runs as a timeline: state, verdict, model, cost, summary.
import { h } from '../../ui/dom.js';
import { chip, emptyState } from '../../ui/components.js';
import { stageLook, verdictLook, modelName, usd } from '../../core/format.js';

const SUMMARY_CLAMP_CHARS = 280;

export function stagesTimeline(detail) {
  const stages = detail.stages ?? [];
  if (stages.length === 0) {
    return emptyState({ title: 'No stages yet', text: 'Stages appear here once the item is dispatched.' });
  }
  const runNumbers = new Map((detail.runs ?? []).map((run, i) => [run.id, i + 1]));
  const multipleRuns = runNumbers.size > 1;
  const list = h('ol', { class: 'timeline', 'aria-label': 'Stages' });
  let lastRun = null;
  for (const stage of stages) {
    if (multipleRuns && stage.run !== lastRun) {
      list.append(h('li', { class: 'timeline-run', text: `Run ${runNumbers.get(stage.run) ?? '?'}` }));
      lastRun = stage.run;
    }
    list.append(stageNode(stage, detail.delegations?.[stage.id]));
  }
  return list;
}

function stageNode(stage, delegation) {
  const look = stageLook(stage);
  const verdict = verdictLook(stage.verdict);
  const name = [stage.stage_id, stage.attempt > 1 && `attempt ${stage.attempt}`, stage.fan_index > 0 && `slot ${stage.fan_index + 1}`]
    .filter(Boolean).join(' · ');
  const meta = [modelName(stage.model_segments?.at(-1)?.choice ?? stage.choice), usd(stage.cost_usd), duration(stage)]
    .filter(Boolean).join(' · ');
  return h('li', { class: ['timeline-node', `tone-${look.tone}`] },
    h('span', { class: 'timeline-dot', 'aria-hidden': 'true' }),
    h('div', { class: 'timeline-body' },
      h('div', { class: 'timeline-head' },
        h('span', { class: 'timeline-name', text: name }),
        h('span', { class: 'chips' }, chip(look), verdict ? chip(verdict) : null)),
      h('p', { class: 'timeline-meta', text: meta }),
      delegation ? h('p', { class: 'timeline-meta', text: `${usd(delegation.direct_usd)} own + ${usd(delegation.delegated_usd)} for ${delegation.agents.length} delegated agent(s)` }) : null,
      stage.state?.error ? h('p', { class: 'timeline-error', text: stage.state.error }) : null,
      stage.summary ? clampedText(stage.summary) : null));
}

function clampedText(text) {
  const long = text.length > SUMMARY_CLAMP_CHARS;
  const body = h('p', { class: 'timeline-summary', text: long ? `${text.slice(0, SUMMARY_CLAMP_CHARS).trimEnd()}…` : text });
  if (!long) return body;
  const more = h('button', { class: 'link-button', type: 'button', 'aria-expanded': 'false' }, 'Show all');
  more.addEventListener('click', () => {
    const open = more.getAttribute('aria-expanded') === 'true';
    body.textContent = open ? `${text.slice(0, SUMMARY_CLAMP_CHARS).trimEnd()}…` : text;
    more.setAttribute('aria-expanded', String(!open));
    more.textContent = open ? 'Show all' : 'Show less';
  });
  return h('div', {}, body, more);
}

function duration(stage) {
  const start = Date.parse(stage.started_at);
  const end = stage.finished_at ? Date.parse(stage.finished_at) : Date.now();
  if (!Number.isFinite(start) || !Number.isFinite(end) || end < start) return '';
  const mins = Math.round((end - start) / 60_000);
  if (mins < 1) return '<1 min';
  return mins < 60 ? `${mins} min` : `${Math.floor(mins / 60)} h ${mins % 60} min`;
}
