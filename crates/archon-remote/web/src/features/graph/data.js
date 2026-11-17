const LIMIT = 500;
const ALL_HISTORY = '1970-01-01T00:00:00Z';
const WEEK_MS = 7 * 24 * 60 * 60 * 1000;

export async function readTaskGraph(cmds, filter = {}, history = 'recent', now = Date.now()) {
  if (typeof cmds?.taskGraph !== 'function') throw new TypeError('A graph reader needs taskGraph');
  if (!['active', 'recent', 'all'].includes(history) || !Number.isFinite(now)) throw new TypeError('Invalid graph history window');
  if (history === 'active') return cmds.taskGraph(filter, LIMIT);
  const since = history === 'all' ? ALL_HISTORY : new Date(now - WEEK_MS).toISOString();
  const [active, recent] = await Promise.all([
    cmds.taskGraph(filter, LIMIT),
    cmds.taskGraph({ ...filter, include_terminal_since: since }, LIMIT),
  ]);
  const merged = mergeGraphs(active, recent);
  if (!merged.truncated) return merged;
  // Each truncated reply only links items it holds; one read of the final set links them all.
  const exact = await cmds.taskGraph({ ...filter, ids: merged.nodes.map((node) => node.id), include_terminal_since: ALL_HISTORY }, LIMIT);
  return withExactEdges(merged, exact);
}

function checkedReply(graph) {
  if (!Array.isArray(graph?.nodes) || !Array.isArray(graph.edges) || !Array.isArray(graph.initiatives)) throw new TypeError('Invalid task graph reply');
  if (graph.nodes.length > LIMIT) throw new RangeError('Task graph exceeded its requested limit');
  return graph;
}

function withExactEdges(merged, exact) {
  checkedReply(exact);
  const ids = new Set(merged.nodes.map((node) => node.id));
  const edges = new Map();
  for (const edge of exact.edges) {
    if (ids.has(edge.from) && ids.has(edge.to)) edges.set(JSON.stringify([edge.from, edge.to, edge.kind]), edge);
  }
  const initiatives = new Map([...merged.initiatives, ...exact.initiatives].map((entry) => [entry.id, entry]));
  return { ...merged, edges: [...edges.values()], initiatives: [...initiatives.values()] };
}

function mergeGraphs(active, recent) {
  checkedReply(active);
  checkedReply(recent);
  const nodes = new Map(active.nodes.map((node) => [node.id, node]));
  for (const node of recent.nodes) nodes.set(node.id, node);
  const selected = [...nodes.values()].slice(0, LIMIT);
  const ids = new Set(selected.map((node) => node.id));
  const edges = new Map();
  for (const edge of [...active.edges, ...recent.edges]) {
    if (ids.has(edge.from) && ids.has(edge.to)) edges.set(JSON.stringify([edge.from, edge.to, edge.kind]), edge);
  }
  const initiatives = new Map([...active.initiatives, ...recent.initiatives].map((entry) => [entry.id, entry]));
  return { nodes: selected, edges: [...edges.values()], initiatives: [...initiatives.values()],
    truncated: active.truncated || recent.truncated || nodes.size > LIMIT };
}
