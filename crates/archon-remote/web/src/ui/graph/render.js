import { usd } from '../../core/format.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const TAGS = new Set(['svg', 'g', 'title', 'rect', 'text', 'circle', 'path', 'polygon']);
const STATES = new Map([
  ['live', 'active'], ['dispatching', 'active'], ['spawning', 'active'], ['running', 'active'], ['active', 'active'], ['working', 'active'],
  ['done', 'success'], ['complete', 'success'], ['completed', 'success'], ['succeeded', 'success'],
  ['blocked', 'warning'], ['waiting', 'warning'], ['pending', 'warning'],
  ['failed', 'danger'], ['error', 'danger'], ['cancelled', 'muted'],
]);
const KINDS = new Set(['depends_on', 'stacked_on', 'initiative_after']);

export function svgElement(tag, attributes = {}, text) {
  if (!TAGS.has(tag)) throw new TypeError(`Unsupported graph SVG element: ${tag}`);
  const element = document.createElementNS(SVG_NS, tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (/^(?:style|on.*|href)$/i.test(name)) throw new TypeError(`Unsafe graph SVG attribute: ${name}`);
    element.setAttribute(name, String(value));
  }
  if (text !== undefined) element.textContent = text;
  return element;
}

function clip(text, length) {
  const points = Array.from(text);
  return points.length > length ? `${points.slice(0, length - 1).join('')}…` : text;
}

function metaText(value) {
  if (value === undefined || value === null) return '';
  const values = Array.isArray(value) ? value : [value];
  if (!values.every((item) => typeof item === 'string' || (typeof item === 'number' && Number.isFinite(item)))) {
    throw new TypeError('Node meta values must be text or finite numbers');
  }
  return values.join(' · ');
}

export function nodeDetails(node) {
  const meta = metaText(node.meta);
  return [node.title || node.id, node.subtitle, node.state, meta, node.cycle ? 'Dependency cycle' : ''].filter(Boolean).join(' · ');
}

export function paintNode(element, node) {
  if (![node.x, node.y, node.width, node.height].every(Number.isFinite)) throw new TypeError('Node bounds must be finite');
  if (node.width <= 0 || node.height <= 0) throw new RangeError('Node dimensions must be positive');
  const description = nodeDetails(node);
  const state = [node.state, node.cycle ? 'cycle' : ''].filter(Boolean).join(' · ');
  element.setAttribute('transform', `translate(${node.x} ${node.y})`);
  element.setAttribute('aria-label', description);
  element.setAttribute('data-state', node.cycle ? 'warning' : STATES.get(node.state.toLowerCase()) ?? 'muted');
  element.replaceChildren(
    svgElement('title', {}, description),
    svgElement('rect', { width: node.width, height: node.height, rx: 12, class: 'graph-node-card' }),
    svgElement('text', { x: 14, y: 23, class: 'graph-node-title' }, clip(node.displayTitle ?? node.title, 29)),
    svgElement('text', { x: 14, y: 42, class: 'graph-node-subtitle' }, clip(node.displaySubtitle ?? node.subtitle, 36)),
    svgElement('circle', { cx: 17, cy: 60, r: 3, class: 'graph-state-dot' }),
    svgElement('text', { x: 26, y: 64, class: 'graph-node-state' }, clip(state, node.cost === undefined ? 32 : 22)),
    svgElement('text', { x: 14, y: 80, class: 'graph-node-meta' }, clip(node.displayMeta ?? metaText(node.meta), 36)),
    svgElement('text', { x: node.width - 14, y: 64, class: 'graph-node-cost', 'text-anchor': 'end' }, node.cost === undefined ? '' : usd(node.cost)),
  );
}

function edgeGeometry(from, to, vertical) {
  if (vertical) return verticalEdge(from, to);
  const x1 = from.x + from.width;
  const y1 = from.y + from.height / 2;
  const y2 = to.y + to.height / 2;
  if (to.x > from.x) {
    const x2 = to.x - 2;
    const middle = (x1 + x2) / 2;
    return { path: `M${x1},${y1} C${middle},${y1} ${middle},${y2} ${x2},${y2}`,
      arrow: `${x2},${y2} ${x2 - 7},${y2 - 4} ${x2 - 7},${y2 + 4}`,
      x: middle, y: (y1 + y2) / 2 - 7 };
  }
  const x2 = to.x + to.width + 2;
  const outside = Math.max(x1, x2) + 30;
  const loopY = from.id === to.id ? y1 - from.height / 2 - 12 : y2;
  return { path: `M${x1},${y1} H${outside} V${loopY} H${x2 + 12} V${y2} H${x2}`,
    arrow: `${x2},${y2} ${x2 + 7},${y2 - 4} ${x2 + 7},${y2 + 4}`,
    x: outside + 6, y: (y1 + loopY) / 2 };
}

export function paintEdges(element, graph) {
  const byId = new Map(graph.nodes.map((node) => [node.id, node]));
  const fragment = document.createDocumentFragment();
  const parallel = parallelOffsets(graph.edges);
  for (const [index, edge] of graph.edges.entries()) {
    const from = byId.get(edge.from);
    const to = byId.get(edge.to);
    if (!from || !to) throw new Error('Rendered edges must reference positioned nodes');
    if (from.width <= 0 || to.width <= 0) throw new Error('Rendered nodes must have positive widths');
    const vertical = graph.direction === 'vertical';
    const axis = vertical ? 'x' : 'y';
    const shift = parallel[index];
    const shape = edgeGeometry({ ...from, [axis]: from[axis] + shift }, { ...to, [axis]: to[axis] + shift }, vertical);
    const label = `${edge.kind}${edge.satisfied === undefined ? '' : edge.satisfied ? ' · satisfied' : ' · unsatisfied'}`;
    const group = svgElement('g', { class: 'graph-edge', role: 'img',
      'aria-label': `${from.title} → ${to.title}: ${label}`, 'data-kind': KINDS.has(edge.kind) ? edge.kind : 'other',
      'data-from': edge.from, 'data-to': edge.to, 'data-satisfied': edge.satisfied === undefined ? 'unknown' : String(edge.satisfied) });
    group.append(svgElement('title', {}, `${from.title} → ${to.title}: ${label}`),
      svgElement('path', { d: shape.path }), svgElement('polygon', { points: shape.arrow }),
      svgElement('text', { x: shape.x, y: shape.y, class: 'graph-edge-label' }, clip(edge.kind, 22)));
    fragment.append(group);
  }
  element.replaceChildren(fragment);
}

function verticalEdge(from, to) {
  const x1 = from.x + from.width / 2;
  const y1 = from.y + from.height;
  const x2 = to.x + to.width / 2;
  const y2 = to.y - 2;
  if (to.y > from.y) {
    const middle = (y1 + y2) / 2;
    return { path: `M${x1},${y1} C${x1},${middle} ${x2},${middle} ${x2},${y2}`,
      arrow: `${x2},${y2} ${x2 - 4},${y2 - 7} ${x2 + 4},${y2 - 7}`, x: (x1 + x2) / 2 + 34, y: middle };
  }
  const outside = Math.max(from.x + from.width, to.x + to.width) + 22;
  return { path: `M${x1},${y1} V${y1 + 16} H${outside} V${y2 - 14} H${x2} V${y2}`,
    arrow: `${x2},${y2} ${x2 - 4},${y2 - 7} ${x2 + 4},${y2 - 7}`, x: outside + 15, y: (y1 + y2) / 2 };
}

function parallelOffsets(edges) {
  const groups = new Map();
  for (const [index, edge] of edges.entries()) {
    const key = JSON.stringify([edge.from, edge.to]);
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(index);
  }
  const offsets = new Array(edges.length).fill(0);
  for (const indices of groups.values()) {
    const step = Math.min(18, 56 / Math.max(1, indices.length - 1));
    indices.forEach((index, position) => { offsets[index] = (position - (indices.length - 1) / 2) * step; });
  }
  return offsets;
}
