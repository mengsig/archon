export const CARD_WIDTH = 240;
export const CARD_HEIGHT = 88;
const COLUMN_GAP = 112;
const ROW_GAP = 36;
const PADDING = 24;

function validText(value, field, empty = true) {
  if (typeof value !== 'string') throw new TypeError(`${field} must be a string`);
  if (!empty && !value.trim()) throw new TypeError(`${field} must not be empty`);
  return value;
}

function validateNode(node, seen) {
  if (!node || typeof node !== 'object') throw new TypeError('Each graph node must be an object');
  validText(node.id, 'Node id', false);
  validText(node.title, 'Node title');
  if (seen.has(node.id)) throw new TypeError(`Duplicate graph node id: ${node.id}`);
  validText(node.subtitle ?? '', 'Node subtitle');
  validText(node.state ?? '', 'Node state');
  for (const key of ['displayTitle', 'displaySubtitle', 'displayMeta']) if (node[key] !== undefined) validText(node[key], key);
  if (node.cost !== undefined && (!Number.isFinite(node.cost) || node.cost < 0)) throw new TypeError('Node cost must be finite and nonnegative');
  seen.add(node.id);
  return { ...node, subtitle: node.subtitle ?? '', state: node.state ?? '' };
}

function validateEdge(edge) {
  if (!edge || typeof edge !== 'object') throw new TypeError('Each graph edge must be an object');
  validText(edge.from, 'Edge from', false);
  validText(edge.to, 'Edge to', false);
  if (edge.kind !== undefined) validText(edge.kind, 'Edge kind');
  if (edge.satisfied !== undefined && typeof edge.satisfied !== 'boolean') {
    throw new TypeError('Edge satisfied must be a boolean');
  }
  return { ...edge, kind: edge.kind ?? 'depends_on' };
}

function adjacency(nodes, edges) {
  const outgoing = new Map(nodes.map((node) => [node.id, []]));
  const incoming = new Map(nodes.map((node) => [node.id, []]));
  const valid = [];
  const dangling = [];
  for (const edge of edges) {
    if (!outgoing.has(edge.from) || !outgoing.has(edge.to)) {
      dangling.push(edge);
      continue;
    }
    outgoing.get(edge.from).push(edge.to);
    incoming.get(edge.to).push(edge.from);
    valid.push(edge);
  }
  return { outgoing, incoming, valid, dangling };
}

function finishingOrder(outgoing) {
  const visited = new Set();
  const order = [];
  for (const start of outgoing.keys()) {
    if (visited.has(start)) continue;
    visited.add(start);
    const stack = [{ id: start, next: 0 }];
    while (stack.length) {
      const frame = stack[stack.length - 1];
      const neighbors = outgoing.get(frame.id);
      if (frame.next === neighbors.length) {
        order.push(frame.id);
        stack.pop();
        continue;
      }
      const next = neighbors[frame.next++];
      if (visited.has(next)) continue;
      visited.add(next);
      stack.push({ id: next, next: 0 });
    }
  }
  if (order.length !== outgoing.size) throw new Error('Graph traversal lost nodes');
  if (visited.size !== order.length) throw new Error('Graph traversal repeated nodes');
  return order;
}

function components(outgoing, incoming) {
  const order = finishingOrder(outgoing);
  const membership = new Map();
  const groups = [];
  for (let index = order.length - 1; index >= 0; index--) {
    const start = order[index];
    if (membership.has(start)) continue;
    const component = groups.length;
    const group = [];
    const stack = [start];
    membership.set(start, component);
    while (stack.length) {
      const id = stack.pop();
      group.push(id);
      for (const next of incoming.get(id)) {
        if (membership.has(next)) continue;
        membership.set(next, component);
        stack.push(next);
      }
    }
    groups.push(group);
  }
  if (membership.size !== outgoing.size) throw new Error('Graph components lost nodes');
  if (groups.some((group) => !group.length)) throw new Error('Empty graph component');
  return { membership, groups };
}

function componentLayers(edges, membership, count) {
  const next = Array.from({ length: count }, () => []);
  const degree = new Array(count).fill(0);
  const layers = new Array(count).fill(0);
  for (const edge of edges) {
    const from = membership.get(edge.from);
    const to = membership.get(edge.to);
    if (from === to) continue;
    next[from].push(to);
    degree[to]++;
  }
  const queue = [];
  for (let index = 0; index < count; index++) if (!degree[index]) queue.push(index);
  for (let cursor = 0; cursor < queue.length; cursor++) {
    const from = queue[cursor];
    for (const to of next[from]) {
      layers[to] = Math.max(layers[to], layers[from] + 1);
      if (--degree[to] === 0) queue.push(to);
    }
  }
  if (queue.length !== count) throw new Error('Component graph must be acyclic');
  if (degree.some((value) => value !== 0)) throw new Error('Unresolved graph component');
  return layers;
}

function placeNodes(nodes, membership, groups, layers, edges) {
  const selfLoops = new Set(edges.filter((edge) => edge.from === edge.to).map((edge) => edge.from));
  const rows = new Map();
  return nodes.map((node) => {
    const component = membership.get(node.id);
    const layer = layers[component];
    const row = rows.get(layer) ?? 0;
    rows.set(layer, row + 1);
    return { ...node, x: PADDING + layer * (CARD_WIDTH + COLUMN_GAP),
      y: PADDING + row * (CARD_HEIGHT + ROW_GAP), width: CARD_WIDTH, height: CARD_HEIGHT,
      layer, cycle: groups[component].length > 1 || selfLoops.has(node.id) };
  });
}

export function layoutGraph({ nodes, edges }, { direction = 'horizontal' } = {}) {
  if (!['horizontal', 'vertical'].includes(direction)) throw new RangeError('Unknown graph direction');
  if (!Array.isArray(nodes)) throw new TypeError('Graph nodes must be an array');
  if (!Array.isArray(edges)) throw new TypeError('Graph edges must be an array');
  const seen = new Set();
  const cleanNodes = nodes.map((node) => validateNode(node, seen));
  const graph = adjacency(cleanNodes, edges.map(validateEdge));
  const { membership, groups } = components(graph.outgoing, graph.incoming);
  const layers = componentLayers(graph.valid, membership, groups.length);
  const positioned = placeNodes(cleanNodes, membership, groups, layers, graph.valid);
  if (direction === 'vertical') {
    for (const node of positioned) {
      const row = (node.y - PADDING) / (CARD_HEIGHT + ROW_GAP);
      node.x = PADDING + row * (CARD_WIDTH + 32);
      node.y = PADDING + node.layer * (CARD_HEIGHT + 64);
    }
  }
  let width = PADDING * 2;
  let height = PADDING * 2;
  for (const node of positioned) {
    width = Math.max(width, node.x + node.width + PADDING + (node.cycle ? 32 : 0));
    height = Math.max(height, node.y + node.height + PADDING);
  }
  return { nodes: positioned, edges: graph.valid, danglingEdges: graph.dangling,
    direction, cycleIds: positioned.filter((node) => node.cycle).map((node) => node.id), width, height };
}
