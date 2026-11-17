const LIMIT = 500;
const WEEK_MS = 7 * 24 * 60 * 60 * 1000;

export async function readTaskGraph(cmds, filter = {}, history = 'recent', now = Date.now()) {
  if (typeof cmds?.taskGraph !== 'function') throw new TypeError('A graph reader needs taskGraph');
  if (!['active', 'recent', 'all'].includes(history) || !Number.isFinite(now)) throw new TypeError('Invalid graph history window');
  if (history === 'active') return cmds.taskGraph(filter, LIMIT);
  const since = history === 'all' ? '1970-01-01T00:00:00Z' : new Date(now - WEEK_MS).toISOString();
  const [active, recent] = await Promise.all([
    cmds.taskGraph(filter, LIMIT),
    cmds.taskGraph({ ...filter, include_terminal_since: since }, LIMIT),
  ]);
  return mergeGraphs(active, recent);
}

function mergeGraphs(active, recent) {
  for (const graph of [active, recent]) {
    if (!Array.isArray(graph?.nodes) || !Array.isArray(graph.edges) || !Array.isArray(graph.initiatives)) throw new TypeError('Invalid task graph reply');
    if (graph.nodes.length > LIMIT) throw new RangeError('Task graph exceeded its requested limit');
  }
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
