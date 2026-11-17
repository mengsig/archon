import test from 'node:test';
import assert from 'node:assert/strict';
import { spendSeries } from '../../src/features/overview/spend.js';
import { taskModel } from '../../src/features/graph/model.js';
import { parseRoute } from '../../src/core/router.js';

const report = (rows) => ({ range: { since: '2026-11-01', until: '2026-11-03' }, rows });
test('daily spend fills absent ledger days and sorts by date', () => {
  const days = spendSeries(report([{ key: '2026-11-03', counted_usd: 4.5 }, { key: '2026-11-01', counted_usd: 1 }]));
  assert.deepEqual(days.map((d) => d.value), [1, 0, 4.5]);
  assert.deepEqual(days.map((d) => d.day), ['2026-11-01', '2026-11-02', '2026-11-03']);
});
test('daily spend rejects bad boundaries and duplicate or negative charges', () => {
  assert.throws(() => spendSeries({ range: { since: 'bad', until: '2026-11-03' }, rows: [] }), /range/);
  assert.throws(() => spendSeries(report([{ key: '2026-11-03', counted_usd: -1 }])), /nonnegative/);
  assert.throws(() => spendSeries(report([{ key: '2026-11-03', counted_usd: 1 }, { key: '2026-11-03', counted_usd: 2 }])), /Duplicate/);
});
test('graph filters preserve edge direction, kind and satisfaction without dangling edges', () => {
  const data = { nodes: [
    { id: 'a', title: 'API', repo: 'one', pipeline: 'build', state: 'succeeded', spent_usd: 1 },
    { id: 'b', title: 'App', repo: 'two', pipeline: 'build', state: 'running', spent_usd: 2 },
  ], edges: [{ from: 'a', to: 'b', kind: 'stacked_on', satisfied: true }] };
  assert.deepEqual(taskModel(data, []).edges, data.edges);
  assert.equal(taskModel(data, [], { repo: 'one' }).edges.length, 0);
  assert.equal(taskModel(data, [{ id: 'two', name: 'website' }], { query: 'website' }).nodes[0].id, 'b');
  assert.equal(taskModel(data, []).nodes[0].state, 'succeeded');
});
test('overview is the landing page and graph is a top-level route', () => {
  assert.equal(parseRoute('').name, 'overview');
  assert.equal(parseRoute('#/graph').tab, 'graph');
  assert.equal(parseRoute('#/queue').name, 'queue');
});
