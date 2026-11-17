import { h } from '../../ui/dom.js';
import { errorNote, skeletonRows, emptyState } from '../../ui/components.js';
import { liveQuery } from '../../core/live-query.js';
import { usd, isTerminal, githubRefUrl } from '../../core/format.js';
import { itemRow, repoNames } from '../queue/item-row.js';
import { openNewItem } from '../new-item/new-item.js';
import { readTaskGraph } from '../graph/data.js';
import { spendChart } from './spend.js';

function panel(title, link = null) {
  return h('section', { class: 'dashboard-panel', 'aria-label': title },
    h('div', { class: 'panel-heading' }, h('h2', { text: title }), link));
}

export function dashboardMetrics(status) {
  const metrics = [
    ['Needs you', status.open_questions, `${status.open_gates} gates · open inbox`, '#/inbox'],
    ['Agents running', status.running_agents, `${status.active_runs} active pipelines`, '#/queue'],
    ['Spend today', usd(status.spend_today_usd), `${usd(status.spend_total_usd)} all time`, '#spend'],
  ];
  return h('div', { class: 'metrics' }, metrics.map(([label, value, note, href]) =>
    h('a', { class: 'metric', href: href === '#spend' ? '#/overview' : href,
      onclick: href === '#spend' ? (event) => { event.preventDefault(); document.getElementById('spend-chart')?.scrollIntoView({ block: 'center' }); } : null },
      h('span', { class: 'metric-label', text: label }), h('strong', { class: 'metric-value', text: String(value) }),
      h('span', { class: 'metric-note', text: note }))));
}

export function activeWork(state) {
  const box = panel('In motion', h('a', { href: '#/queue', text: 'View queue ↗' }));
  const items = (state.summaries ?? []).filter((item) => !isTerminal(item)).slice(0, 3);
  box.append(items.length ? h('ul', { class: 'item-list', 'aria-label': 'Active work' },
    items.map((item) => itemRow(item, repoNames(state.repos), Date.now())))
    : emptyState({ title: 'A clear runway', text: 'Your next piece of work starts in the queue.' }));
  return box;
}

export function quickActions(ctx) {
  return h('div', { class: 'quick-actions' },
    h('a', { class: 'button button-plain', href: '#/graph' }, 'Explore graph'),
    h('button', { class: 'button button-primary', type: 'button', onclick: () => openNewItem(ctx) }, 'New item'));
}

export function dashboardInsights(ctx) {
  const spend = panel('Spend over time');
  spend.id = 'spend-chart';
  const chart = h('div', {}, skeletonRows(2));
  spend.append(chart);
  const relationships = panel('Across your workspace', h('a', { href: '#/graph', text: 'Open graph ↗' }));
  const links = h('div', { class: 'relationship-list' }, skeletonRows(2));
  relationships.append(links);
  const cost = liveQuery(ctx, () => ctx.cmds.costReport('day'), ({ data, error }) => {
    chart.replaceChildren(error ? errorNote(error, () => cost.refresh()) : spendChart(data));
  });
  const graph = liveQuery(ctx, () => readTaskGraph(ctx.cmds), ({ data, error }) => {
    links.replaceChildren(...(error ? [errorNote(error, () => graph.refresh())] : workspaceLinks(data, ctx.store.get().repos)));
  });
  return { spend, relationships, destroy() { cost.destroy(); graph.destroy(); } };
}

function workspaceLinks(data, repos) {
  if (!Array.isArray(data?.initiatives) || !Array.isArray(data?.nodes)) throw new TypeError('Workspace relationships need a task graph');
  const names = repoNames(repos);
  const links = data.initiatives.slice(0, 4).map((initiative) => {
    const items = data.nodes.filter((node) => node.initiative === initiative.id);
    const repositories = [...new Set(items.map((node) => names.get(node.repo) ?? 'No repository'))];
    return h('a', { class: 'relationship-card', href: '#/graph' }, h('strong', { text: initiative.title }),
      h('small', { text: `${items.length} items · ${repositories.join(', ')} · ${initiative.state}` }));
  });
  for (const node of data.nodes.filter((node) => node.pr).slice(-4).reverse()) {
    const url = githubRefUrl(node.pr);
    if (!url) continue;
    links.push(h('a', { class: 'relationship-card', href: url, target: '_blank', rel: 'noopener noreferrer' },
      h('strong', { text: node.title }), h('small', { text: `Pull request #${node.pr.number} · ${names.get(node.repo) ?? 'Repository'} ↗` })));
  }
  if (!links.length) links.push(emptyState({ title: 'Work is taking shape', text: 'Initiatives and pull requests appear here as your work progresses.' }));
  if (data.truncated) links.push(h('p', { class: 'section-foot', text: 'Active work is prioritised within 500 items; some relationships are omitted.' }));
  return links;
}
