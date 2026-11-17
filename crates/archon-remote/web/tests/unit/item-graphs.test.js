import test from 'node:test';
import assert from 'node:assert/strict';
import { workGraph, agentGraph, liveAgentGraph } from '../../src/features/item/graph-models.js';
import { itemPanels } from '../../src/features/item/graphs.js';
import { createStore } from '../../src/core/store.js';
import { domHarness } from '../support/graph-dom.js';

const choice = { model: { provider: 'codex', model: 'gpt-6' }, effort: 'high' };
const started_at = '2026-11-17T10:00:00Z';
const finished_at = '2026-11-17T10:02:00Z';
const run = (id = 'run1') => ({ id, pipeline: 'build', state: { state: 'running' }, started_at, cost_usd: 3 });
const stage = (id, overrides = {}) => ({
  id, run: 'run1', stage_id: 'build', attempt: 1, fan_index: 0, choice,
  aether_agent: `agt_${id}`, state: { state: 'completed' }, cost_usd: 3,
  started_at, finished_at, ...overrides,
});
const summary = (id, overrides = {}) => ({ id, kind: 'work', fan_out: 1, gate: 'none', effort: 'medium', ...overrides });
const plan = (stages, run = 'run1') => ({ run, pipeline: 'build', stages, settings: [] });
const detail = (stages = [], overrides = {}) => ({ runs: [run()], stages, gates: [], ...overrides });
const delegated = (agent, parent, overrides = {}) => ({ agent, parent, ...choice, cost_usd: 0.5, ...overrides });
const split = (agents) => ({ direct_usd: 1, delegated_usd: 2, agents });
const node = (graph, predicate) => {
  const found = graph.nodes.filter(predicate);
  assert.equal(found.length, 1);
  return found[0];
};
const links = (graph, from, to) => graph.edges.filter((edge) => edge.from === from.id && edge.to === to.id);

function validGraph(graph) {
  const ids = new Set(graph.nodes.map((entry) => entry.id));
  assert.equal(ids.size, graph.nodes.length);
  for (const entry of graph.nodes) {
    for (const field of ['id', 'title', 'subtitle', 'state']) assert.equal(typeof entry[field], 'string');
    if (entry.meta != null) assert.equal(typeof entry.meta, 'string');
  }
  for (const edge of graph.edges) {
    assert.ok(ids.has(edge.from));
    assert.ok(ids.has(edge.to));
  }
}

test('empty detail creates empty graphs and frozen plans create not-started stages', () => {
  assert.deepEqual(workGraph(detail([], { runs: [] })), { nodes: [], edges: [] });
  assert.deepEqual(agentGraph(detail([], { runs: [] })), { nodes: [], edges: [] });
  const graph = workGraph(detail(), [plan([summary('build'), summary('review', { kind: 'review', fan_out: 3, gate: 'all_pass' })])]);
  const build = node(graph, (n) => n.pipelineStageId === 'build');
  const review = node(graph, (n) => n.pipelineStageId === 'review');
  assert.equal(build.state, 'Not started');
  assert.match(review.subtitle, /3 slot/);
  assert.match(review.meta, /Gate: all pass/);
  assert.equal(links(graph, build, review)[0].kind, 'depends_on');
  validGraph(graph);
});

test('fan-out slots and repeated retries attach to planned stages without false sequencing', () => {
  const records = [stage('a'), stage('b', { fan_index: 1 }), stage('c', { attempt: 2 }), stage('d', { attempt: 2 })];
  const graph = workGraph(detail(records), [plan([summary('build', { fan_out: 2 }), summary('ship')])]);
  const attempts = graph.nodes.filter((n) => n.stageId);
  assert.equal(attempts.length, 4);
  assert.equal(new Set(attempts.map((n) => n.id)).size, 4);
  assert.equal(graph.edges.filter((e) => e.kind === 'attempt').length, 4);
  assert.ok(graph.edges.every((edge) => !attempts.some((n) => n.id === edge.from)));
  assert.equal(node(graph, (n) => n.stageId === 'b').slot, 1);
  assert.equal(node(graph, (n) => n.stageId === 'c').attempt, 2);
  validGraph(graph);
});

test('historical runs with the same pipeline stage and agent remain distinct', () => {
  const records = [stage('a'), stage('b', { run: 'run2', aether_agent: 'agt_a' })];
  const data = detail(records, { runs: [run(), run('run2')] });
  const graph = workGraph(data, [plan([summary('build')]), plan([summary('build')], 'run2')]);
  assert.equal(graph.nodes.filter((n) => n.pipelineStageId === 'build').length, 4);
  const agents = agentGraph(data);
  assert.equal(agents.nodes.length, 2);
  assert.notEqual(agents.nodes[0].id, agents.nodes[1].id);
  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  assert.ok(graph.edges.every((e) => byId.get(e.from).runId === byId.get(e.to).runId));
  validGraph(graph);
  validGraph(agents);
});

test('missing plans preserve actual stages without invented stage dependencies', () => {
  const graph = workGraph(detail([stage('a'), stage('b', { stage_id: 'review' })]));
  assert.match(graph.nodes[0].meta, /Plan unavailable/);
  assert.equal(graph.nodes.filter((n) => n.stageId).length, 2);
  assert.ok(graph.edges.every((e) => e.kind === 'contains'));
  const orphan = workGraph(detail([stage('c', { run: 'old' })], { runs: [] }));
  assert.equal(orphan.nodes.length, 2);
  assert.equal(orphan.nodes[0].runId, 'old');
  validGraph(orphan);
});

test('plans absent from run history and unplanned actual stages stay visible', () => {
  const graph = workGraph(detail([stage('a', { stage_id: 'legacy' })], { runs: [] }), [plan([summary('future')], 'run2')]);
  assert.equal(graph.nodes.filter((n) => n.runId === 'run2').length, 2);
  assert.equal(node(graph, (n) => n.pipelineStageId === 'future').state, 'Not started');
  assert.equal(node(graph, (n) => n.stageId === 'a').pipelineStageId, 'legacy');
  validGraph(graph);
});

test('operator gates match exact run, stage and attempt, including fan-out', () => {
  const gates = [
    { run: 'run1', stage_id: 'build', attempt: 2, subject: 'report', summary: '<script>plain</script>', opened_at: started_at },
    { run: 'run1', stage_id: 'build', attempt: 1, subject: 'proposal', decision: { decision: 'approve' }, opened_at: started_at, decided_at: finished_at },
    { run: 'run2', stage_id: 'build', attempt: 1, subject: 'report', withdrawn: 'Run cancelled', opened_at: started_at, decided_at: finished_at },
  ];
  const graph = workGraph(detail([stage('a'), stage('b', { attempt: 2 }), stage('c', { attempt: 2, fan_index: 1 })], { gates }));
  const open = node(graph, (n) => n.gateIndex === 0);
  assert.equal(open.state, 'Awaiting operator');
  assert.match(open.meta, /<script>plain<\/script>/);
  assert.equal(graph.edges.filter((e) => e.to === open.id).length, 2);
  assert.ok(graph.edges.filter((e) => e.to === open.id).every((e) => !e.satisfied));
  assert.equal(node(graph, (n) => n.gateIndex === 2).state, 'Withdrawn');
  assert.ok(graph.edges.find((e) => e.to === node(graph, (n) => n.gateIndex === 1).id).satisfied);
  validGraph(graph);
});

test('metadata uses actual current model, effort, split costs and elapsed timing', () => {
  const current = { model: { provider: 'other', model: '<b>model</b>' }, effort: 'x_high' };
  const data = detail([stage('a', { model_segments: [{ choice: current }] })], { delegations: { a: split([]) } });
  const actual = node(workGraph(data), (n) => n.stageId === 'a');
  assert.match(actual.meta, /other\/<b>model<\/b> · x_high/);
  assert.match(actual.meta, /\$1.00 own \+ \$2.00 delegated/);
  assert.match(actual.meta, /120 s elapsed/);
  assert.equal(actual.agentId, 'agt_a');
  const active = node(workGraph(detail([stage('a', { finished_at: null })])), (n) => n.stageId === 'a');
  assert.match(active.meta, /Started 2026-11-17T10:00:00Z · In progress/);
});

test('frozen model settings appear on future pipeline stages', () => {
  const frozen = { ...plan([summary('build')]), settings: [{ key: 'build', choices: [choice] }] };
  const graph = workGraph(detail(), [frozen]);
  assert.match(node(graph, (n) => n.pipelineStageId === 'build').meta, /codex\/gpt-6 · high/);
  assert.equal(graph.nodes.length, 2);
});

test('agent graph preserves exact deep parents regardless of input order', () => {
  const data = detail([stage('a')], { delegations: { a: split([
    delegated('grandchild', 'child'), delegated('child', 'agt_a'),
  ]) } });
  const orchestrator = { agent: 'orch', status: 'ready', ...choice };
  const graph = agentGraph(data, orchestrator);
  const lead = node(graph, (n) => n.agentId === 'agt_a');
  const child = node(graph, (n) => n.agentId === 'child');
  const grandchild = node(graph, (n) => n.agentId === 'grandchild');
  assert.equal(links(graph, lead, child)[0].kind, 'delegates');
  assert.equal(links(graph, child, grandchild)[0].kind, 'delegates');
  assert.equal(links(graph, node(graph, (n) => n.agentId === 'orch'), lead)[0].kind, 'manages');
  assert.equal(child.parentAgentId, 'agt_a');
  assert.equal(grandchild.state, 'State unavailable');
  assert.equal(child.state, 'State unavailable');
  assert.equal(lead.state, 'completed');
  assert.match(child.meta, /codex\/gpt-6 · high · \$0.50/);
  validGraph(graph);
});

test('missing parents and absent stage leads remain explicit disconnected nodes', () => {
  const data = detail([stage('a', { aether_agent: null })], { delegations: { a: split([delegated('child', 'missing')]) } });
  const graph = agentGraph(data);
  assert.equal(graph.nodes.length, 1);
  assert.equal(graph.edges.length, 0);
  assert.equal(graph.nodes[0].missingParent, true);
  assert.match(graph.nodes[0].subtitle, /Missing parent: missing/);
  validGraph(graph);
});

test('cycles and self-parent records terminate and retain recorded relationships', () => {
  const graph = agentGraph(detail([stage('a')], { delegations: { a: split([
    delegated('one', 'two'), delegated('two', 'one'), delegated('self', 'self'),
  ]) } }));
  assert.equal(graph.nodes.length, 4);
  assert.equal(graph.edges.length, 3);
  assert.equal(links(graph, node(graph, (n) => n.agentId === 'one'), node(graph, (n) => n.agentId === 'two')).length, 1);
  validGraph(graph);
});

test('unknown lifecycle and absent model or effort remain visible without invented status', () => {
  const graph = agentGraph(detail([stage('a', { state: {}, choice: null })], {
    delegations: { a: split([delegated('child', 'agt_a', { effort: null })]) },
  }), { agent: 'orch' });
  assert.equal(node(graph, (n) => n.agentId === 'orch').state, 'State unavailable');
  assert.equal(node(graph, (n) => n.agentId === 'agt_a').state, 'State unavailable');
  assert.match(node(graph, (n) => n.agentId === 'child').meta, /Effort unavailable/);
  assert.match(node(graph, (n) => n.agentId === 'agt_a').meta, /Model unavailable/);
});

test('adapters do not mutate inputs and return stable output for the same snapshot', () => {
  const data = detail([stage('a')], { delegations: { a: split([delegated('child', 'agt_a')]) } });
  const plans = [plan([summary('build')])];
  const original = JSON.stringify({ data, plans });
  const deepFreeze = (value) => {
    Object.freeze(value);
    for (const child of Object.values(value)) if (child && typeof child === 'object') deepFreeze(child);
    assert.ok(Object.isFrozen(value));
    return value;
  };
  deepFreeze(data);
  deepFreeze(plans);
  assert.deepEqual(workGraph(data, plans), workGraph(data, plans));
  assert.deepEqual(agentGraph(data), agentGraph(data));
  assert.equal(JSON.stringify({ data, plans }), original);
});

test('encoded IDs cannot collide through delimiters or different record categories', () => {
  const records = [stage('run1'), stage('b', { stage_id: 'x:y' }), stage('c', { run: 'run1:x', stage_id: 'y' })];
  const graph = workGraph(detail(records), [plan([summary('x:y')]), plan([summary('y')], 'run1:x')]);
  validGraph(graph);
  assert.equal(new Set(graph.nodes.map((n) => n.id)).size, graph.nodes.length);
});

test('malformed boundaries reject duplicate IDs, invalid dimensions, costs and timestamps', () => {
  assert.throws(() => workGraph(null), /ItemDetail/);
  assert.throws(() => workGraph(detail([], { runs: [run(), run()] })), /Duplicate runs/);
  assert.throws(() => workGraph(detail([stage('a'), stage('a')])), /Duplicate stages/);
  assert.throws(() => workGraph(detail(), [plan([]), plan([])]), /Duplicate plans/);
  assert.throws(() => workGraph(detail(), [plan([summary('a'), summary('a')])]), /Duplicate plan.stages/);
  assert.throws(() => workGraph(detail([stage('a', { attempt: 0 })])), /attempt/);
  assert.throws(() => workGraph(detail([stage('a', { fan_index: -1 })])), /fan_index/);
  assert.throws(() => workGraph(detail(), [plan([summary('a', { fan_out: 0 })])]), /fan_out/);
  assert.throws(() => workGraph(detail([stage('a', { cost_usd: NaN })])), /cost/);
  assert.throws(() => workGraph(detail([stage('a', { finished_at: '2026-11-16T10:00:00Z' })])), /timestamp/);
  assert.throws(() => workGraph(detail([stage('a', { started_at: 'invalid' })])), /timestamp/);
  assert.throws(() => agentGraph(detail([stage('a')], { delegations: { a: split([delegated('child', 'a'), delegated('child', 'a')]) } })), /Duplicate delegated/);
  assert.throws(() => agentGraph(detail([stage('a')], { delegations: { a: split([delegated('agt_a', 'other')]) } })), /lead repeated/);
});

test('revision and rejection gates remain unsatisfied and unknown states stay readable', () => {
  const gates = ['revise', 'reject'].map((decision, index) => ({
    run: 'run1', stage_id: 'build', attempt: index + 1, subject: 'report',
    decision: { decision }, opened_at: started_at, decided_at: finished_at,
  }));
  const graph = workGraph(detail([stage('a', { state: { state: 'waiting_for_tool' } })], { gates }));
  assert.equal(node(graph, (n) => n.gateIndex === 0).state, 'revise');
  assert.equal(node(graph, (n) => n.gateIndex === 1).state, 'reject');
  assert.ok(graph.edges.filter((e) => e.kind === 'operator_gate').every((e) => e.satisfied === false));
  assert.equal(node(graph, (n) => n.stageId === 'a').state, 'waiting for tool');
  validGraph(graph);
});

test('delegated lifecycle remains unavailable while the stage is running', () => {
  const graph = agentGraph(detail([stage('a', { state: { state: 'running' }, finished_at: null })], {
    delegations: { a: split([delegated('child', 'agt_a')]) },
  }));
  assert.equal(node(graph, (n) => n.agentId === 'agt_a').state, 'running');
  assert.equal(node(graph, (n) => n.agentId === 'child').state, 'State unavailable');
  validGraph(graph);
});

test('invalid collections and model boundaries remain visible errors', () => {
  assert.throws(() => workGraph({ stages: {} }), /stages must be an array/);
  assert.throws(() => workGraph(detail(), {}), /plans must be an array/);
  assert.throws(() => workGraph(detail([], { gates: {} })), /gates must be an array/);
  assert.throws(() => workGraph(detail([stage('a', { choice: { model: { provider: 'codex' } } })])), /model.model/);
  assert.throws(() => agentGraph(detail(), { agent: '' }), /orchestrator.agent/);
  assert.throws(() => agentGraph(detail([stage('a')], { delegations: { a: split([delegated('child', '')]) } })), /agent.parent/);
  const frozen = { ...plan([summary('a')]), settings: [{ key: 'a', choices: {} }] };
  assert.throws(() => workGraph(detail(), [frozen]), /settings.choices/);
});

test('a reused agent keeps each attempt’s own charge; its lifetime spend is not shown on history', () => {
  const records = [
    stage('a1', { aether_agent: 'agt_reused', attempt: 1, cost_usd: 1.25 }),
    stage('a2', { aether_agent: 'agt_reused', attempt: 2, cost_usd: 2.5 }),
  ];
  const graph = liveAgentGraph(detail(records), null, [{ agent: 'agt_reused', state: 'live', cost_usd: 99 }]);
  const leads = graph.nodes.filter((entry) => entry.agentId === 'agt_reused');
  assert.deepEqual(leads.map((entry) => entry.cost).sort(), [1.25, 2.5]);
  for (const lead of leads) {
    assert.equal(lead.state, 'live');
    assert.doesNotMatch(lead.meta, /99|direct recorded spend/);
  }
  assert.throws(() => liveAgentGraph(detail(records), null, null), /states are a list/);
});

// The Agents tab as the item page mounts it: its loader reads item_agent_states,
// whose cost_usd is lifetime spend, and its overlay lays only the state over the tree.
test('the Agents view keeps a reused agent’s per-attempt costs over a lifetime cost_usd', async () => {
  const harness = domHarness();
  Object.assign(document, { visibilityState: 'visible', addEventListener() {}, removeEventListener() {} });
  const stream = () => ({ subscribe: () => () => {} });
  const asked = [];
  const ctx = {
    store: createStore({ connection: { state: 'open' }, status: null }), feed: stream(), changes: stream(),
    cmds: { itemAgentStates: async (item) => {
      asked.push(item);
      return { agents: [{ agent: 'agt_reused', state: 'live' }], cost_usd: { agt_reused: 99 } };
    } },
  };
  const records = [
    stage('a1', { aether_agent: 'agt_reused', attempt: 1, cost_usd: 1.25 }),
    stage('a2', { aether_agent: 'agt_reused', attempt: 2, cost_usd: 2.5 }),
  ];
  const panels = itemPanels(ctx, 'qi_1');
  panels.update(detail(records, { item: { id: 'qi_1', state: { state: 'running' } } }));
  const agentsTab = panels.element.query('button').find((button) => button.dataset.tab === 'agents');
  agentsTab.dispatch('click');
  await new Promise((resolve) => setImmediate(resolve));
  harness.resize();
  harness.flush();
  assert.deepEqual(asked, ['qi_1'], 'the tab read the agent states once');
  const agentsPanel = panels.element.query('div').find((panel) => panel.attrs.get('id') === 'item-agents');
  const cards = agentsPanel.query('g').filter((g) => g.className === 'graph-node');
  const reused = cards.filter((card) => /agt_reused/.test(card.getAttribute('aria-label')));
  assert.equal(reused.length, 2, 'one card per attempt of the reused agent');
  const costs = reused.map((card) => card.query('text').find((t) => t.className === 'graph-node-cost').textContent);
  assert.deepEqual(costs.sort(), ['$1.25', '$2.50']);
  for (const card of reused) {
    assert.match(card.getAttribute('aria-label'), /live/);
    assert.doesNotMatch(card.getAttribute('aria-label'), /99/);
  }
  panels.destroy();
});
