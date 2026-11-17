import test from 'node:test';
import assert from 'node:assert/strict';
import { readTaskGraph } from '../../src/features/graph/data.js';

const reply = (nodes, edges = [], truncated = false) => ({ nodes, edges, initiatives: [], truncated });

test('active work survives more than 500 recently finished items', async () => {
  const old = Array.from({ length: 501 }, (_, i) => ({ id: `old${i}`, state: 'succeeded' }));
  const active = [{ id: 'current', state: 'running' }, { id: 'next', state: 'queued' }];
  const calls = [];
  const result = await readTaskGraph({ taskGraph: async (filter, limit) => {
    calls.push({ filter, limit });
    return filter.include_terminal_since ? reply(old.slice(0, limit), [], true)
      : reply(active, [{ from: 'current', to: 'next', kind: 'depends_on', satisfied: false }]);
  } }, { repos: ['repo'] }, 'recent', Date.parse('2026-11-17T00:00:00Z'));
  assert.equal(result.nodes.length, 500);
  assert.deepEqual(result.nodes.slice(0, 2), active);
  assert.equal(result.edges.length, 1);
  assert.equal(result.truncated, true);
  assert.equal(calls[0].filter.include_terminal_since, undefined);
  assert.equal(calls[1].filter.include_terminal_since, '2026-11-10T00:00:00.000Z');
  assert.ok(calls.every(({ filter, limit }) => filter.repos[0] === 'repo' && limit === 500));
});

test('history keeps completed prerequisites and deduplicates typed edges', async () => {
  const active = reply([{ id: 'b', state: 'running' }]);
  const history = reply([{ id: 'a', state: 'succeeded' }, { id: 'b', state: 'completed' }],
    [{ from: 'a', to: 'b', kind: 'depends_on', satisfied: true }]);
  const result = await readTaskGraph({ taskGraph: async (filter) => filter.include_terminal_since ? history : active });
  assert.equal(result.nodes.length, 2);
  assert.equal(result.nodes[0].state, 'completed');
  assert.equal(result.edges[0].satisfied, true);
  assert.equal(result.truncated, false);
});

test('active-only history makes one bounded read and failures remain failures', async () => {
  const calls = [];
  const cmds = { taskGraph: async (...args) => { calls.push(args); return reply([]); } };
  assert.deepEqual((await readTaskGraph(cmds, {}, 'active')).nodes, []);
  assert.deepEqual(calls, [[{}, 500]]);
  await assert.rejects(readTaskGraph(cmds, {}, 'yesterday'), /Invalid graph history/);
  await assert.rejects(readTaskGraph({ taskGraph: async () => { throw new Error('offline'); } }), /offline/);
});
