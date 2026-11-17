import { h } from '../../ui/dom.js';
import { errorNote, skeletonRows, emptyState } from '../../ui/components.js';
import { createGraph } from '../../ui/graph/graph.js';
import { workGraph, liveAgentGraph } from './graph-models.js';
import { liveQuery, MIN_POLL_MS } from '../../core/live-query.js';
import { isTerminal } from '../../core/format.js';
import { stagesTimeline } from './stages.js';
import { openSheet } from '../../ui/sheet.js';
import { renderMarkdown } from '../../ui/markdown.js';

export function itemPanels(ctx, id) {
  const details = h('div', { class: 'item-panel', role: 'tabpanel', id: 'item-details', 'aria-labelledby': 'tab-details', dataset: { panel: 'details' } });
  const work = h('div', { class: 'item-panel', role: 'tabpanel', id: 'item-work', 'aria-labelledby': 'tab-work', hidden: true });
  const agents = h('div', { class: 'item-panel', role: 'tabpanel', id: 'item-agents', 'aria-labelledby': 'tab-agents', hidden: true });
  const panels = { details, work, agents };
  const graphs = graphViews(ctx, id, work, agents);
  let selected = 'details';
  const select = (name) => {
    if (!panels[name]) throw new RangeError('Unknown item tab');
    selected = name;
    for (const [key, panel] of Object.entries(panels)) panel.hidden = key !== name;
    for (const button of tabs.children) {
      button.setAttribute('aria-selected', String(button.dataset.tab === name));
      button.tabIndex = button.dataset.tab === name ? 0 : -1;
    }
    graphs.activate(name);
    if (name !== 'details') tabs.scrollIntoView({ block: 'start', behavior: 'instant' });
  };
  const tabs = h('div', { class: 'item-tabs', role: 'tablist', 'aria-label': 'Item views' },
    Object.keys(panels).map((name) => h('button', {
      class: 'item-tab', type: 'button', role: 'tab', id: `tab-${name}`, dataset: { tab: name },
      'aria-controls': `item-${name}`, 'aria-selected': String(name === selected), tabindex: name === selected ? 0 : -1,
      onclick: () => select(name), onkeydown: (event) => tabKey(event, tabs, select),
    }, name[0].toUpperCase() + name.slice(1))));
  return { element: h('div', { class: 'item-view-content' }, tabs, details, work, agents), details,
    update: (detail) => graphs.update(detail), destroy: () => graphs.destroy() };
}

function tabKey(event, tabs, select) {
  const step = { ArrowRight: 1, ArrowLeft: -1, Home: -Infinity, End: Infinity }[event.key];
  if (step === undefined) return;
  event.preventDefault();
  const buttons = [...tabs.children];
  const index = buttons.indexOf(event.currentTarget);
  const next = step === -Infinity ? 0 : step === Infinity ? buttons.length - 1 : (index + step + buttons.length) % buttons.length;
  buttons[next].focus();
  select(buttons[next].dataset.tab);
}

function graphViews(ctx, id, work, agents) {
  const workGraphView = createGraph({ label: 'Pipeline work graph', onSelect: (node) => inspectNode(ctx, id, current, node) });
  const agentGraphView = createGraph({ label: 'Agent tree', onSelect: (node) => inspectNode(ctx, id, current, node) });
  const planError = h('div');
  const agentError = h('div');
  const timeline = h('div');
  work.append(planError, workGraphView.element, timeline);
  agents.append(agentError, agentGraphView.element,
    h('p', { class: 'section-foot', text: 'Solid links show parent relationships. “Manages” links show orchestration. Live state is read from Aether; missing state is labelled unavailable.' }));
  let current = null;
  let plans = [];
  let states = [];
  let resource = null;
  let active = 'details';
  let polling = false;
  const draw = () => {
    if (!current) return;
    if (active === 'work') {
      workGraphView.update(workGraph(current, plans));
      timeline.replaceChildren(stagesTimeline(current));
    }
    if (active === 'agents') agentGraphView.update(liveAgentGraph(current, ctx.store.get().status?.orchestrator, states));
  };
  const load = () => loadGraphData(ctx, id, current, active);
  // Delegated agents start inside a stage with no event to say so: poll them while the item runs.
  const startQuery = () => {
    resource?.destroy();
    polling = active === 'agents' && Boolean(current) && !isTerminal(current.item);
    resource = active === 'details' ? null : liveQuery(ctx, load, receive, { interval: polling ? MIN_POLL_MS : undefined });
    draw();
  };
  const receive = ({ data, error }) => {
    const notes = active === 'work' ? planError : agentError;
    notes.replaceChildren(...(error ? [errorNote(error, () => resource.refresh())] : []));
    if (error && active === 'agents') states = [];
    if (data?.plans) plans = data.plans;
    if (data?.states) states = data.states;
    draw();
  };
  return {
    update(detail) {
      current = detail;
      // An item that finishes (or is retried) restarts the agents query with or without the poll.
      if (active === 'agents' && polling === isTerminal(detail.item)) return startQuery();
      draw();
      if (active !== 'details') resource?.refresh();
    },
    activate(name) {
      active = name;
      startQuery();
    },
    destroy() { resource?.destroy(); workGraphView.destroy(); agentGraphView.destroy(); },
  };
}

async function loadGraphData(ctx, id, detail, active) {
  if (!detail) return {};
  if (detail.item.id !== id || !Array.isArray(detail.runs)) throw new TypeError('Graph detail must belong to the current item');
  if (active === 'work') return { plans: await Promise.all(detail.runs.map((run) => ctx.cmds.runPlan(run.id))) };
  if (active === 'agents') {
    const result = await ctx.cmds.itemAgentStates(id);
    if (!Array.isArray(result?.agents)) throw new TypeError('Agent state reply needs agents');
    const ids = new Set();
    for (const entry of result.agents) {
      if (typeof entry.agent !== 'string' || !entry.agent || ids.has(entry.agent)
          || !['live', 'completed', 'stopped'].includes(entry.state)) throw new TypeError('Invalid or duplicate agent state');
      ids.add(entry.agent);
    }
    return { states: result.agents };
  }
  return {};
}

function inspectNode(ctx, id, detail, node) {
  const content = h('div', { class: 'graph-detail' }, h('p', { text: node.state }),
    h('p', { text: node.subtitle }), h('p', { text: node.meta }));
  const stage = node.parentAgentId ? null : detail?.stages.find((stage) => stage.id === node.stageId);
  if (stage) content.append(stagesTimeline({ ...detail, stages: [stage] }));
  const sheet = openSheet({ title: node.title, content });
  if (!stage) return;
  const output = h('div', {}, skeletonRows(2));
  content.append(output);
  ctx.cmds.stage(id, stage.run, stage.stage_id, stage.attempt, stage.fan_index).then((data) => {
    if (sheet.closed) return;
    const found = data.stages.find((entry) => entry.attempt === stage.attempt && entry.slot === stage.fan_index);
    output.replaceChildren(found?.output?.text ? renderMarkdown(found.output.text)
      : emptyState({ title: 'No output yet', text: 'The agent’s output appears as it is recorded.' }));
  }, (error) => { if (!sheet.closed) output.replaceChildren(errorNote(error)); });
}
