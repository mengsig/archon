import { usd } from '../../core/format.js';

export function taskModel(data, repos, { repo = '', initiative = '', query = '' } = {}) {
  if (!Array.isArray(data?.nodes) || !Array.isArray(data?.edges)) throw new TypeError('Task graph needs nodes and edges');
  const names = new Map((repos ?? []).map((entry) => [entry.id, entry.name]));
  const search = query.trim().toLowerCase();
  const nodes = data.nodes.filter((node) => (!repo || node.repo === repo)
    && (!initiative || node.initiative === initiative)
    && (!search || `${node.title} ${node.id} ${names.get(node.repo) ?? ''}`.toLowerCase().includes(search)));
  const ids = new Set(nodes.map((node) => node.id));
  if (ids.size !== nodes.length) throw new TypeError('Task graph returned duplicate item ids');
  return {
    nodes: nodes.map((node) => ({
      id: node.id, title: node.title, state: node.state,
      subtitle: `${names.get(node.repo) ?? 'No repository'} · ${node.pipeline}`,
      meta: [node.stage, usd(node.spent_usd)].filter(Boolean).join(' · '), item: node,
    })),
    edges: data.edges.filter((edge) => ids.has(edge.from) && ids.has(edge.to)),
  };
}
