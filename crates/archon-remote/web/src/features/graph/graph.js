import { h, replace } from '../../ui/dom.js';
import { screenHeader, errorNote, skeletonRows, emptyState, chip } from '../../ui/components.js';
import { createGraph } from '../../ui/graph/graph.js';
import { liveQuery } from '../../core/live-query.js';
import { stateLook, usd, githubRefUrl } from '../../core/format.js';
import { taskModel } from './model.js';
import { readTaskGraph } from './data.js';

export function mountGraph(view, ctx) {
  const status = h('div', { role: 'status', class: 'graph-intro' });
  const errors = h('div');
  const detail = h('aside', { class: 'graph-detail', hidden: true, 'aria-label': 'Selected item' });
  let model = null;
  const graph = createGraph({ label: 'Item dependencies', onSelect: (node) => showItem(detail, node.item, model) });
  const filters = { repo: '', initiative: '', query: '', history: 'recent' };
  let data = null;
  const redraw = () => {
    if (!data) return;
    model = taskModel(data, ctx.store.get().repos, filters);
    graph.update(model);
    status.replaceChildren(h('span', { text: `${model.nodes.length} items · ${model.edges.length} connections` }),
      h('span', { text: data.truncated ? '500-item limit: some items and connections omitted. Active work is prioritised; narrow filters to see more.' : 'Active work first · live from your workspace' }));
    if (!model.nodes.length) status.append(emptyState({ title: 'No connected work to show', text: 'Queue an item or clear your filters.' }));
  };
  const controls = graphFilters(ctx, filters, (reload) => { detail.hidden = true; if (reload) resource.refresh(); redraw(); });
  view.append(screenHeader({ title: 'Graph' }), controls.element, status, errors,
    h('div', { class: 'graph-workspace' }, graph.element, detail),
    h('p', { class: 'section-foot', text: 'Arrows point from prerequisite to dependent. Drag to pan, pinch to zoom, or use the graph controls. Filters show only connections between visible items.' }));
  const resource = liveQuery(ctx, () => readTaskGraph(ctx.cmds, {
    ...(filters.repo ? { repos: [filters.repo] } : {}),
    ...(filters.initiative ? { initiative: filters.initiative } : {}),
  }, filters.history), (result) => {
    errors.replaceChildren(...(result.error ? [errorNote(result.error, () => resource.refresh())] : []));
    if (result.data) { data = result.data; controls.initiatives(data.initiatives ?? []); redraw(); }
  });
  status.append(skeletonRows(2));
  const off = ctx.store.watch((s) => s.repos, () => { controls.repos(); redraw(); });
  return () => { resource.destroy(); off(); graph.destroy(); };
}

function graphFilters(ctx, filters, change) {
  const repo = h('select', { class: 'select-chip', 'aria-label': 'Graph repository' });
  const initiative = h('select', { class: 'select-chip', 'aria-label': 'Graph initiative' });
  const search = h('input', { class: 'search-input', type: 'search', placeholder: 'Find an item', 'aria-label': 'Search graph' });
  search.addEventListener('input', () => { filters.query = search.value; change(false); });
  const history = h('select', { class: 'select-chip', 'aria-label': 'Graph history' },
    [['active', 'Active only'], ['recent', 'Active + last 7 days'], ['all', 'All history · active first']].map(([value, text]) => h('option', { value, text })));
  history.value = filters.history;
  for (const [key, control] of [['repo', repo], ['initiative', initiative], ['history', history]]) {
    control.addEventListener('change', () => { filters[key] = control.value; change(true); });
  }
  const options = (select, values, chosen, label) => {
    select.replaceChildren(h('option', { value: '', text: label }), ...values.map(([value, text]) => h('option', { value, text })));
    select.value = chosen;
  };
  options(initiative, [], '', 'All initiatives');
  return {
    element: h('div', { class: 'graph-filters' }, h('label', { class: 'search' }, search), repo, initiative, history),
    repos: () => options(repo, (ctx.store.get().repos ?? []).map((r) => [r.id, r.name]), filters.repo, 'All repositories'),
    initiatives: (values) => options(initiative, values.map((i) => [i.id, i.title]), filters.initiative, 'All initiatives'),
  };
}

// The selected item's facts and its visible connections, all from the graph already read.
function showItem(container, item, model) {
  if (!item?.id || typeof item.title !== 'string') throw new TypeError('A selected graph node needs its item');
  if (!Array.isArray(model?.nodes) || !Array.isArray(model?.edges)) throw new TypeError('A selected item is shown with its graph');
  const look = stateLook({ state: { state: item.state } });
  const open = h('a', { class: 'button button-primary', href: `#/item/${item.id}` }, 'Open item');
  container.hidden = false;
  replace(container, h('h2', { text: item.title }), chip(look),
    item.state_detail ? h('p', { text: item.state_detail }) : null,
    itemFacts(item, look),
    connections(item.id, model),
    open,
    h('button', { class: 'link-button', type: 'button', onclick: () => { container.hidden = true; } }, 'Dismiss'));
  open.focus();
}

function itemFacts(item, look) {
  const url = githubRefUrl(item.pr);
  const fact = (term, value) => [h('dt', { text: term }), h('dd', {}, value)];
  // replace() skips an absent pull request; replaceChildren would print it as "null".
  return h('dl', { class: 'graph-detail-facts' },
    fact('Item', h('code', { text: item.id })),
    fact('Pipeline', item.pipeline || '—'),
    fact('State', look.label),
    item.stage ? fact('Stage', item.stage) : null,
    fact('Spent', usd(item.spent_usd)),
    url ? fact('Pull request', h('a', { href: url, target: '_blank', rel: 'noopener noreferrer' }, `${item.pr.repo}#${item.pr.number}`)) : null);
}

const EDGE_WORDS = { depends_on: 'depends on', stacked_on: 'stacked on', onto: 'onto pull request', initiative_after: 'initiative order' };

// Prerequisites point at the item and dependents away from it, as the arrows do.
function connections(id, model) {
  const titles = new Map(model.nodes.map((node) => [node.id, node.title]));
  const entry = (other, edge) => h('li', {},
    h('a', { href: `#/item/${other}`, text: titles.get(other) ?? other }),
    h('span', { class: 'graph-detail-kind', text: `${EDGE_WORDS[edge.kind] ?? edge.kind}${edge.satisfied === false ? ' · unsatisfied' : ''}` }));
  const group = (title, entries) => (entries.length ? [h('h3', { text: title }), h('ul', {}, ...entries)] : []);
  const prerequisites = model.edges.filter((edge) => edge.to === id).map((edge) => entry(edge.from, edge));
  const dependents = model.edges.filter((edge) => edge.from === id).map((edge) => entry(edge.to, edge));
  if (!prerequisites.length && !dependents.length) return h('p', { text: 'No dependencies among the items shown.' });
  return h('div', { class: 'graph-detail-deps' }, ...group('Prerequisites', prerequisites), ...group('Dependents', dependents));
}
