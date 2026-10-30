// Overview: daemon and orchestrator health, agents against caps, spend
// against budgets, the global pause, repositories and pipelines.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { screenHeader, section, card, row, chip, toggle, errorNote, skeletonRows, describeError } from '../../ui/components.js';
import { toast } from '../../ui/toast.js';
import { usd, dateTime } from '../../core/format.js';
import { reposCard, pipelinesCard } from './catalog.js';

export function mountOverview(view, ctx) {
  const settings = h('a', { class: 'icon-button', href: '#/settings', 'aria-label': 'Settings' }, icon('settings', { size: 22 }));
  const body = h('div', { class: 'overview' }, skeletonRows(4));
  const pipelines = { data: null, error: null };
  view.append(screenHeader({ title: 'Overview', actions: [settings] }), body);

  const render = () => {
    const state = ctx.store.get();
    if (!state.status) {
      body.replaceChildren(state.errors.status ? errorNote(state.errors.status, () => ctx.refresh.one('status')) : skeletonRows(4));
      return;
    }
    // A failed refresh stays in view above the last status it replaced.
    const stale = state.errors.status ? [errorNote(state.errors.status, () => ctx.refresh.one('status'))] : [];
    body.replaceChildren(...stale,
      pauseCard(ctx, state.status),
      section('Health', healthCard(state.status)),
      section('Agents', agentsCard(state.status)),
      section('Spend', spendCard(state.status, state.cost, state.repos),
        state.errors.cost ? errorNote(state.errors.cost, () => ctx.refresh.one('cost')) : null),
      section('Repositories', state.errors.repos
        ? card(errorNote(state.errors.repos, () => ctx.refresh.one('repos')))
        : reposCard(ctx, state.repos, state.status.pause)),
      section('Pipelines', pipelinesCard(pipelines, state.status.pause, () => loadPipelines())));
  };
  const loadPipelines = async () => {
    try {
      pipelines.data = await ctx.cmds.pipelines(null);
      pipelines.error = null;
    } catch (err) {
      pipelines.error = err;
    }
    render();
  };
  const off = ctx.store.watch((s) => [s.status, s.cost, s.repos, s.errors.status, s.errors.cost, s.errors.repos], render);
  ctx.refresh.many(['status', 'cost', 'repos']);
  loadPipelines();
  return off;
}

function pauseCard(ctx, status) {
  const paused = status.pause.global;
  return h('div', { class: ['hero-card', paused ? 'is-paused' : 'is-running'] },
    h('div', { class: 'hero-state' },
      h('span', { class: 'hero-dot', 'aria-hidden': 'true' }),
      h('span', { class: 'hero-label', text: paused ? 'Paused' : 'Running' })),
    h('p', { class: 'hero-text', text: paused ? 'Nothing new starts. Running agents finish their stages.' : `${status.queued_items} queued · ${status.running_agents} agent${status.running_agents === 1 ? '' : 's'} running` }),
    toggle({
      label: 'Pause new work', checked: paused,
      description: 'A soft pause: dispatch stops, nothing is killed.',
      onChange: async (on) => {
        await (on ? ctx.cmds.pause({ scope: 'global' }) : ctx.cmds.resume({ scope: 'global' }));
        toast(on ? 'Archon is paused' : 'Archon is running again', { tone: 'green' });
        await ctx.refresh.one('status');
      },
      onError: (err) => toast(describeError(err), { tone: 'red' }),
    }));
}

function healthCard(status) {
  const aether = status.aether.state === 'connected'
    ? chip({ label: 'Connected', tone: 'green' })
    : chip({ label: 'Disconnected', tone: 'amber' });
  const orchestrator = status.orchestrator
    ? `${status.orchestrator.status} · ${status.orchestrator.model?.provider ?? ''}/${status.orchestrator.model?.model ?? ''}`
    : 'Not started';
  return card(
    row('Aether', aether),
    row('Orchestrator', orchestrator),
    status.disk?.held ? row('Disk', chip({ label: 'Dispatch held: low space', tone: 'amber' })) : null,
    status.recovery ? row('Recovery', `${status.recovery.runs_done}/${status.recovery.runs_total ?? '?'} runs`) : null,
    status.upgrade ? row('Upgrade', status.upgrade) : null,
    row('Archon', status.daemon_version),
    row('Up since', dateTime(status.started_at)));
}

function agentsCard(status) {
  return card(
    meterRow('Agents running', status.running_agents, status.caps.max_parallel_agents),
    meterRow('Active runs', status.active_runs, status.caps.max_active_runs),
    h('div', { class: 'stat-grid' },
      stat('Queued', status.queued_items),
      stat('Blocked', status.blocked_items, status.blocked_items ? 'amber' : null),
      stat('Questions', status.open_questions, status.open_questions ? 'amber' : null),
      stat('Gates', status.open_gates, status.open_gates ? 'amber' : null)));
}

function spendCard(status, cost, repos) {
  const caps = status.caps;
  const rows = [
    meterRow('Today', status.spend_today_usd, caps.budgets_enabled ? caps.budget_usd_daily : 0, usd),
    meterRow('All time', status.spend_total_usd, caps.budgets_enabled ? caps.budget_usd_total : 0, usd),
  ];
  if (cost) {
    rows.push(row(`Last ${daysIn(cost.range)} days`, usd(cost.total.counted_usd)));
    const names = new Map((repos ?? []).map((r) => [r.id, r.name]));
    for (const entry of cost.rows.slice(0, 6)) rows.push(row(names.get(entry.key) ?? entry.key ?? 'Orchestrator and other', usd(entry.counted_usd), { tone: 'muted' }));
  }
  if (!caps.budgets_enabled) rows.push(h('p', { class: 'card-foot', text: 'Dollar budgets are off; spend is still recorded.' }));
  return card(...rows);
}

function daysIn(range) {
  const days = Math.round((Date.parse(range.until) - Date.parse(range.since)) / 86_400_000) + 1;
  return Number.isFinite(days) && days > 0 ? days : 7;
}

// A value against its cap; a cap of 0 means unlimited and draws no bar.
function meterRow(label, value, cap, format = String) {
  const limited = cap > 0;
  const share = limited ? Math.min(1, value / cap) : 0;
  const tone = share >= 1 ? 'red' : share >= 0.8 ? 'amber' : 'accent';
  const fill = h('div', { class: ['meter-fill', `tone-bg-${tone}`] });
  fill.style.width = `${Math.round(share * 100)}%`;
  return h('div', { class: 'meter-row' },
    h('div', { class: 'row' }, h('span', { class: 'row-label', text: label }),
      h('span', { class: 'row-value', text: limited ? `${format(value)} of ${format(cap)}` : `${format(value)} · no limit` })),
    limited ? h('div', { class: 'meter', role: 'meter', 'aria-label': label, 'aria-valuemin': '0', 'aria-valuemax': String(cap), 'aria-valuenow': String(value) }, fill) : null);
}

function stat(label, value, tone = null) {
  return h('div', { class: 'stat' },
    h('span', { class: ['stat-value', tone && `tone-text-${tone}`], text: String(value) }),
    h('span', { class: 'stat-label', text: label }));
}
