import { conversationStandIn } from './conversation.js';

const at = '2026-11-17T07:00:00Z';
const choice = { model: { provider: 'codex', model: 'gpt-6' }, effort: 'high' };
const runId = 'pr_graph_fixture';

export function pipelineFixture(base) {
  const stage = (id, stage_id, state, extra = {}) => ({
    id, run: runId, stage_id, attempt: 1, fan_index: 0, choice,
    state: { state }, cost_usd: 1.25, aether_agent: `agt_${id}`,
    started_at: at, finished_at: state === 'completed' ? '2026-11-17T07:02:00Z' : null, ...extra,
  });
  const stages = [stage('plan', 'plan', 'completed'), stage('build', 'build', 'running'),
    stage('review0', 'review', 'completed'), stage('review1', 'review', 'running', { fan_index: 1, attempt: 2 })];
  const detail = { ...base, item: { ...base.item, state: { state: 'running', stage: 'build' } },
    runs: [{ id: runId, pipeline: 'adaptive', state: { state: 'running' }, started_at: at, finished_at: null, cost_usd: 5 }],
    stages, spent_usd: 5, gates: [{ run: runId, stage_id: 'plan', attempt: 1, subject: 'proposal',
      opened_at: at, decided_at: '2026-11-17T07:01:00Z', decision: { decision: 'approve' }, withdrawn: null }],
    delegations: { build: { direct_usd: 0.75, delegated_usd: 0.5, agents: [
      { agent: 'agt_reviewer', parent: 'agt_build', model: { provider: 'claude-code', model: 'fable-5.2' }, effort: 'high', cost_usd: 0.3 },
      { agent: 'agt_check', parent: 'agt_reviewer', model: { provider: 'codex', model: 'gpt-6' }, effort: 'medium', cost_usd: 0.2 },
    ] } },
  };
  const plan = { run: runId, pipeline: 'adaptive', revision: 'fixture', settings: [], stages:
    ['plan', 'build', 'review', 'ship'].map((id) => ({ id, kind: 'work', fan_out: id === 'review' ? 2 : 1, gate: 'none', effort: 'high' })) };
  return { detail, plan, agentStates: { agents: [{ agent: 'agt_reviewer', state: 'live' }, { agent: 'agt_check', state: 'completed' }],
    cost_usd: { agt_build: 0.75, agt_reviewer: 0.3, agt_check: 0.2, agt_orchestrator: 1.1 } },
    orchestrator: { agent: 'agt_orchestrator', ...choice, status: 'running' }, requests: [] };
}

// Real daemon for all other requests; Aether-owned pipeline records are explicit stand-ins.
export async function standInGraphs(page, fixture, entries = []) {
  const conversation = conversationStandIn(entries);
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    const statusRequests = new Set();
    const answer = (frame, reply, data) => ws.send(JSON.stringify({ frame: 'reply', id: frame.req, result: { Ok: { reply, data } } }));
    server.onMessage((message) => {
      const frame = JSON.parse(message);
      if (statusRequests.delete(frame.id) && frame.result?.Ok?.reply === 'status') frame.result.Ok.data.orchestrator = fixture.orchestrator;
      ws.send(JSON.stringify(frame));
    });
    ws.onMessage((message) => {
      const frame = JSON.parse(message);
      fixture.requests.push(frame);
      if (frame.cmd === 'conversation') return conversation.answer(ws, frame);
      if (frame.cmd === 'get_item' && frame.item === fixture.detail.item.id) return answer(frame, 'item_detail', fixture.detail);
      if (frame.cmd === 'run_plan') return answer(frame, 'run_plan', fixture.plan);
      if (frame.cmd === 'item_agent_states' && fixture.stateError) return ws.send(JSON.stringify({ frame: 'reply', id: frame.req, result: { Err: { code: 'unavailable', message: fixture.stateError } } }));
      if (frame.cmd === 'item_agent_states') return answer(frame, 'item_agent_states', fixture.agentStates);
      if (frame.cmd === 'get_stage') return answer(frame, 'stage_detail', { item: frame.item, run: frame.run,
        stages: [{ stage_id: frame.stage, attempt: frame.attempt, slot: frame.slot, output: { text: 'Verified stage output.', offset: 0, total_chars: 22 } }] });
      if (frame.cmd === 'task_graph' && fixture.graph) return answer(frame, 'task_graph', fixture.graph);
      if (frame.cmd === 'status') statusRequests.add(frame.req);
      if (frame.cmd === 'say') conversation.echoSay(frame);
      server.send(message);
    });
  });
}
