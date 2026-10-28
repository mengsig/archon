// The queue: every item, filtered by state, repository, pipeline, priority
// and search.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { reconcile } from '../../ui/list.js';
import { screenHeader, emptyState, errorNote, skeletonRows } from '../../ui/components.js';
import { FILTERS, PRIORITIES, priorityLabel } from '../../core/format.js';
import { itemRow, rowSignature, repoNames } from './item-row.js';
import { openNewItem } from '../new-item/new-item.js';

// Kept across visits, so coming back from an item finds the same view.
const viewState = { filter: 'active', repo: '', pipeline: '', priority: '', query: '' };
const AGE_TICK_MS = 60_000;

export function mountQueue(view, ctx) {
  const { store } = ctx;
  const chips = h('div', { class: 'filter-chips', role: 'group', 'aria-label': 'Filter by state' });
  const repoSelect = h('select', { class: 'select-chip', 'aria-label': 'Repository' });
  const pipelineSelect = h('select', { class: 'select-chip', 'aria-label': 'Pipeline' });
  const prioritySelect = h('select', { class: 'select-chip', 'aria-label': 'Priority' });
  const selects = { repo: repoSelect, pipeline: pipelineSelect, priority: prioritySelect };
  const search = h('input', { class: 'search-input', type: 'search', placeholder: 'Search', 'aria-label': 'Search items', value: viewState.query, enterkeyhint: 'search' });
  const list = h('ul', { class: 'item-list', 'aria-label': 'Items' });
  const status = h('div', { class: 'list-status' });
  const fab = h('button', { class: 'fab', type: 'button', 'aria-label': 'New item', onclick: () => openNewItem(ctx) }, icon('plus', { size: 26 }));

  view.append(
    screenHeader({ title: 'Queue' }),
    h('div', { class: 'toolbar' },
      h('label', { class: 'search' }, icon('search', { size: 18 }), search),
      repoSelect),
    h('div', { class: 'toolbar toolbar-filters' }, pipelineSelect, prioritySelect),
    chips, status, list, fab);

  const render = () => renderQueue({ state: store.get(), chips, selects, list, status, ctx });
  const off = store.watch((s) => [s.summaries, s.repos, s.errors.summaries], render);
  search.addEventListener('input', () => { viewState.query = search.value; render(); });
  for (const [key, select] of Object.entries(selects)) {
    select.addEventListener('change', () => { viewState[key] = select.value; render(); });
  }
  chips.addEventListener('click', (event) => {
    const button = event.target.closest('[data-filter]');
    if (!button) return;
    viewState.filter = button.dataset.filter;
    render();
  });
  const ticker = setInterval(render, AGE_TICK_MS);
  return () => { off(); clearInterval(ticker); };
}

function renderQueue({ state, chips, selects, list, status, ctx }) {
  const names = repoNames(state.repos);
  const summaries = state.summaries;
  renderSelects(selects, state.repos, summaries);
  const scoped = (summaries ?? []).filter((s) => matchesSelects(s) && matchesQuery(s, names));
  renderChips(chips, scoped);
  const active = FILTERS.find((f) => f.id === viewState.filter) ?? FILTERS[0];
  const shown = scoped.filter(active.test);
  const now = Date.now();
  reconcile(list, shown, {
    key: (s) => s.id,
    signature: (s) => rowSignature(s, names, now),
    render: (s) => itemRow(s, names, now),
  });
  status.replaceChildren();
  if (state.errors.summaries) status.append(errorNote(state.errors.summaries, () => ctx.refresh.one('summaries')));
  if (summaries === null && !state.errors.summaries) status.append(skeletonRows(4));
  else if (summaries && shown.length === 0) status.append(emptyFor(active.id, summaries.length));
}

function matchesSelects(summary) {
  return (!viewState.repo || summary.repo === viewState.repo)
    && (!viewState.pipeline || summary.pipeline === viewState.pipeline)
    && (!viewState.priority || summary.priority === viewState.priority);
}

function matchesQuery(summary, names) {
  const query = viewState.query.trim().toLowerCase();
  if (!query) return true;
  return [summary.title, summary.id, summary.pipeline, names.get(summary.repo)]
    .some((text) => String(text ?? '').toLowerCase().includes(query));
}

function renderChips(chips, scoped) {
  const counts = FILTERS.map((f) => scoped.filter(f.test).length);
  const signature = JSON.stringify([viewState.filter, counts]);
  if (chips.dataset.sig === signature) return;
  chips.dataset.sig = signature;
  chips.replaceChildren(...FILTERS.map((f, i) => h('button', {
    class: 'filter-chip', type: 'button', dataset: { filter: f.id }, 'aria-pressed': String(f.id === viewState.filter),
  }, f.label, h('span', { class: 'filter-count', text: String(counts[i]) }))));
}

function renderSelects(selects, repos, summaries) {
  const pipelines = [...new Set((summaries ?? []).map((s) => s.pipeline))].sort();
  renderSelect(selects.repo, 'repo', 'All repos', (repos ?? []).map((repo) => [repo.id, repo.name]));
  renderSelect(selects.pipeline, 'pipeline', 'All pipelines', pipelines.map((name) => [name, name]));
  renderSelect(selects.priority, 'priority', 'Any priority', PRIORITIES.map((p) => [p, priorityLabel(p)]));
}

// `options` are [value, label] pairs; the chosen one stays even when absent.
function renderSelect(select, key, allLabel, options) {
  const chosen = viewState[key];
  if (chosen && !options.some(([value]) => value === chosen)) options.push([chosen, chosen]);
  const signature = JSON.stringify([options, chosen]);
  if (select.dataset.sig === signature) return;
  select.dataset.sig = signature;
  select.replaceChildren(h('option', { value: '', text: allLabel }),
    ...options.map(([value, label]) => h('option', { value, text: label, selected: value === chosen })));
  select.value = chosen;
}

function emptyFor(filter, total) {
  if (viewState.query) return emptyState({ title: 'No matches', text: 'Try another search or filter.', iconName: 'search' });
  if (total === 0) return emptyState({ title: 'Nothing queued yet', text: 'Tap + to give Archon its first piece of work.', iconName: 'queue' });
  const copy = {
    active: ['Nothing active', 'Everything queued has finished.'],
    waiting: ['Nothing needs you', 'Questions and gates show up here.'],
    blocked: ['Nothing held up', 'Items waiting on a dependency, a gate or the host show up here.'],
    done: ['Nothing done yet', 'Finished items collect here.'],
    failed: ['No failures', 'Items that fail show up here.'],
    all: ['Nothing here', ''],
  }[filter];
  return emptyState({ title: copy[0], text: copy[1], iconName: 'check' });
}
