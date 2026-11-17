const UNKNOWN = 'State unavailable';
const graphId = (...parts) => JSON.stringify(parts);
const words = (value) => typeof value === 'string' && value.length ? value.replaceAll('_', ' ') : UNKNOWN;
const stateOf = (record) => words(record?.state?.state);

function text(value, label) {
  if (typeof value !== 'string' || !value.trim()) throw new TypeError(`${label} must be non-empty text`);
  return value;
}

function unique(records, key, label) {
  if (!Array.isArray(records)) throw new TypeError(`${label} must be an array`);
  const result = new Map();
  for (const record of records) {
    if (!record || typeof record !== 'object') throw new TypeError(`${label} entries must be objects`);
    const id = text(record[key], `${label}.${key}`);
    if (result.has(id)) throw new TypeError(`Duplicate ${label} ID: ${id}`);
    result.set(id, record);
  }
  return result;
}

function input(detail) {
  if (!detail || typeof detail !== 'object') throw new TypeError('ItemDetail must be an object');
  const runs = unique(detail.runs ?? [], 'id', 'runs');
  const stages = unique(detail.stages ?? [], 'id', 'stages');
  if (!Array.isArray(detail.gates ?? [])) throw new TypeError('gates must be an array');
  for (const stage of stages.values()) {
    text(stage.run, 'stage.run');
    text(stage.stage_id, 'stage.stage_id');
    if (!Number.isInteger(stage.attempt) || stage.attempt < 1) throw new TypeError('stage.attempt must be positive');
    if (!Number.isInteger(stage.fan_index) || stage.fan_index < 0) throw new TypeError('stage.fan_index must be non-negative');
    money(stage.cost_usd);
    if (!runs.has(stage.run)) runs.set(stage.run, { id: stage.run });
  }
  for (const gate of detail.gates ?? []) {
    text(gate.run, 'gate.run');
    text(gate.stage_id, 'gate.stage_id');
    if (!Number.isInteger(gate.attempt) || gate.attempt < 1) throw new TypeError('gate.attempt must be positive');
    if (!runs.has(gate.run)) runs.set(gate.run, { id: gate.run });
  }
  return { runs, stages };
}

function checkedGraph(graph) {
  const nodes = unique(graph.nodes, 'id', 'graph nodes');
  for (const edge of graph.edges) {
    if (!nodes.has(edge.from) || !nodes.has(edge.to)) throw new Error('Graph edge has a missing endpoint');
  }
  return graph;
}

function money(value) {
  if (value == null) return 'Cost unavailable';
  if (!Number.isFinite(value) || value < 0) throw new TypeError('cost must be finite and non-negative');
  return `$${value.toFixed(2)}`;
}

function modelMeta(choice) {
  const model = choice?.model;
  if (!model) return `Model unavailable · ${choice?.effort ?? 'Effort unavailable'}`;
  text(model.provider, 'model.provider');
  text(model.model, 'model.model');
  return `${model.provider}/${model.model} · ${choice.effort ?? 'Effort unavailable'}`;
}

function timing(started, finished) {
  if (!started) return 'Timing unavailable';
  const start = Date.parse(started);
  if (!Number.isFinite(start)) throw new TypeError('Invalid start timestamp');
  if (!finished) return `Started ${started} · In progress`;
  const end = Date.parse(finished);
  if (!Number.isFinite(end) || end < start) throw new TypeError('Invalid finish timestamp');
  return `${Math.round((end - start) / 1000)} s elapsed`;
}

function stageNode(stage, delegation) {
  const choice = stage.model_segments?.at(-1)?.choice ?? stage.choice;
  const cost = delegation
    ? `${money(delegation.direct_usd)} own + ${money(delegation.delegated_usd)} delegated`
    : money(stage.cost_usd);
  return {
    id: graphId('stage', stage.id), title: stage.stage_id, cost: stage.cost_usd,
    displaySubtitle: `Attempt ${stage.attempt} · Slot ${stage.fan_index + 1}`, displayMeta: timing(stage.started_at, stage.finished_at),
    subtitle: `Attempt ${stage.attempt} · Slot ${stage.fan_index + 1} · Run ${stage.run}`,
    state: stateOf(stage), meta: `${modelMeta(choice)} · ${cost} · ${timing(stage.started_at, stage.finished_at)}`,
    stageId: stage.id, pipelineStageId: stage.stage_id, runId: stage.run,
    agentId: stage.aether_agent ?? null, attempt: stage.attempt, slot: stage.fan_index,
  };
}

function indexStages(stages) {
  const byRun = new Map();
  const byStage = new Map();
  const byAttempt = new Map();
  for (const stage of stages.values()) {
    for (const [index, key] of [
      [byRun, stage.run], [byStage, graphId(stage.run, stage.stage_id)],
      [byAttempt, graphId(stage.run, stage.stage_id, stage.attempt)],
    ]) {
      if (!index.has(key)) index.set(key, []);
      index.get(key).push(stage);
    }
  }
  return { byRun, byStage, byAttempt };
}

function addPlan(graph, plan, index) {
  const summaries = unique(plan.stages, 'id', 'plan.stages');
  const settings = unique(plan.settings ?? [], 'key', 'plan.settings');
  let previous = null;
  for (const summary of summaries.values()) {
    if (!Number.isInteger(summary.fan_out) || summary.fan_out < 1) throw new TypeError('fan_out must be positive');
    const id = graphId('planned', plan.run, summary.id);
    const attempts = index.byStage.get(graphId(plan.run, summary.id)) ?? [];
    const choices = settings.get(summary.id)?.choices ?? [];
    if (!Array.isArray(choices)) throw new TypeError('settings.choices must be an array');
    graph.nodes.push({
      id, title: summary.id, displaySubtitle: `${summary.kind} · ${summary.fan_out} slot(s)`, subtitle: `${summary.kind} · ${summary.fan_out} slot(s) · Run ${plan.run}`,
      state: attempts.length ? 'Started' : 'Not started',
      meta: [`Gate: ${words(summary.gate ?? 'none')}`, ...choices.map(modelMeta),
        !choices.length && `Default effort: ${summary.effort ?? 'unavailable'}`].filter(Boolean).join(' · '),
      runId: plan.run, pipelineStageId: summary.id,
    });
    graph.edges.push({ from: previous ?? graphId('run', plan.run), to: id, kind: previous ? 'depends_on' : 'contains' });
    for (const stage of attempts) graph.edges.push({ from: id, to: graphId('stage', stage.id), kind: 'attempt' });
    previous = id;
  }
  return summaries;
}

function addGates(graph, gates, index) {
  for (const [position, gate] of gates.entries()) {
    const id = graphId('gate', gate.run, position);
    const state = gate.withdrawn != null ? 'Withdrawn' : gate.decision ? words(gate.decision.decision) : 'Awaiting operator';
    graph.nodes.push({
      id, title: `${words(gate.subject)} gate`, subtitle: `${gate.stage_id} · Attempt ${gate.attempt} · Run ${gate.run}`,
      state, meta: [gate.summary, gate.withdrawn, timing(gate.opened_at, gate.decided_at)].filter(Boolean).join(' · '),
      runId: gate.run, pipelineStageId: gate.stage_id, attempt: gate.attempt, gateIndex: position,
    });
    const attempts = index.byAttempt.get(graphId(gate.run, gate.stage_id, gate.attempt)) ?? [];
    const parents = attempts.length ? attempts.map((stage) => graphId('stage', stage.id)) : [graphId('run', gate.run)];
    for (const from of parents) graph.edges.push({ from, to: id, kind: 'operator_gate', satisfied: gate.decision?.decision === 'approve' });
  }
}

export function workGraph(detail, plans = []) {
  const { runs, stages } = input(detail);
  const frozen = unique(plans, 'run', 'plans');
  const index = indexStages(stages);
  const graph = { nodes: [], edges: [] };
  for (const plan of frozen.values()) if (!runs.has(plan.run)) runs.set(plan.run, { id: plan.run, pipeline: plan.pipeline });
  for (const run of runs.values()) {
    const plan = frozen.get(run.id);
    graph.nodes.push({
      id: graphId('run', run.id), title: `Run ${run.id}`, displayTitle: `${run.pipeline ?? 'Pipeline'} run`, cost: run.cost_usd, subtitle: run.pipeline ?? plan?.pipeline ?? 'Pipeline unavailable',
      state: stateOf(run), meta: `${plan ? 'Frozen plan' : 'Plan unavailable'} · ${money(run.cost_usd)} · ${timing(run.started_at, run.finished_at)}`,
      runId: run.id,
    });
    const summaries = plan ? addPlan(graph, plan, index) : new Map();
    for (const stage of index.byRun.get(run.id) ?? []) {
      graph.nodes.push(stageNode(stage, detail.delegations?.[stage.id]));
      if (!summaries.has(stage.stage_id)) graph.edges.push({ from: graphId('run', run.id), to: graphId('stage', stage.id), kind: 'contains' });
    }
  }
  addGates(graph, detail.gates ?? [], index);
  return checkedGraph(graph);
}

function addDelegates(graph, stage, delegation, leadId) {
  const agents = unique(delegation.agents, 'agent', 'delegated agents');
  money(delegation.direct_usd);
  money(delegation.delegated_usd);
  for (const agent of agents.values()) {
    text(agent.parent, 'agent.parent');
    if (agent.agent === stage.aether_agent) throw new TypeError('Stage lead repeated as delegated agent');
    const parentId = agent.parent === stage.aether_agent ? leadId
      : agents.has(agent.parent) ? graphId('agent', stage.id, agent.parent) : null;
    graph.nodes.push({
      id: graphId('agent', stage.id, agent.agent), title: agent.agent, cost: agent.cost_usd,
      displayTitle: 'Delegated agent', displaySubtitle: modelMeta(agent), displayMeta: agent.agent,
      subtitle: parentId ? `Delegated · ${stage.stage_id} · Run ${stage.run}` : `Missing parent: ${agent.parent} · Run ${stage.run}`,
      state: UNKNOWN, meta: `${modelMeta(agent)} · ${money(agent.cost_usd)}`,
      agentId: agent.agent, parentAgentId: agent.parent, stageId: stage.id, runId: stage.run,
      pipelineStageId: stage.stage_id, missingParent: !parentId,
    });
    if (parentId) graph.edges.push({ from: parentId, to: graphId('agent', stage.id, agent.agent), kind: 'delegates' });
  }
}

export function agentGraph(detail, orchestrator = null) {
  const { stages } = input(detail);
  const graph = { nodes: [], edges: [] };
  if (orchestrator) {
    text(orchestrator.agent, 'orchestrator.agent');
    graph.nodes.push({
      id: graphId('orchestrator', orchestrator.agent), title: 'Orchestrator', subtitle: orchestrator.agent,
      displaySubtitle: modelMeta(orchestrator), displayMeta: 'Workspace orchestrator',
      state: words(orchestrator.status), meta: modelMeta(orchestrator), agentId: orchestrator.agent,
    });
  }
  for (const stage of stages.values()) {
    const delegation = detail.delegations?.[stage.id];
    let leadId = null;
    if (stage.aether_agent) {
      text(stage.aether_agent, 'stage.aether_agent');
      leadId = graphId('agent', stage.id, stage.aether_agent);
      const lead = stageNode(stage, delegation);
      graph.nodes.push({ ...lead, id: leadId, title: stage.aether_agent,
        displayTitle: `${stage.stage_id} · lead`, displaySubtitle: modelMeta(stage.model_segments?.at(-1)?.choice ?? stage.choice),
        displayMeta: `Attempt ${stage.attempt} · Slot ${stage.fan_index + 1}`, cost: delegation?.direct_usd ?? stage.cost_usd,
        subtitle: `${stage.stage_id} · Attempt ${stage.attempt} · Slot ${stage.fan_index + 1} · Run ${stage.run}` });
      if (orchestrator) graph.edges.push({ from: graphId('orchestrator', orchestrator.agent), to: leadId, kind: 'manages' });
    }
    if (delegation) addDelegates(graph, stage, delegation, leadId);
  }
  return checkedGraph(graph);
}

// The agent tree with each agent's live state laid over it. Costs stay the per-attempt
// figures: an agent's recorded spend is lifetime, so it would misprice reused agents.
export function liveAgentGraph(detail, orchestrator, states) {
  if (!Array.isArray(states)) throw new TypeError('agent states are a list');
  const graph = agentGraph(detail, orchestrator);
  const byAgent = new Map(states.map((agent) => [agent.agent, agent.state]));
  for (const node of graph.nodes) {
    if (byAgent.has(node.agentId)) node.state = byAgent.get(node.agentId);
  }
  return graph;
}
