import { h } from '../dom.js';
import { animateMove } from '../motion.js';
import { layoutGraph } from './layout.js';
import { svgElement, nodeDetails, paintNode, paintEdges } from './render.js';
import { createCameraController } from './controller.js';

function updateWarnings(warnings, graph) {
  const messages = [];
  if (graph.cycleIds.length) messages.push(`Dependency cycle: ${graph.cycleIds.join(', ')}`);
  for (const edge of graph.danglingEdges) messages.push(`Missing endpoint: ${edge.from} → ${edge.to} (${edge.kind})`);
  warnings.hidden = messages.length === 0;
  const summary = [graph.cycleIds.length ? `${graph.cycleIds.length} cyclic nodes` : '',
    graph.danglingEdges.length ? `${graph.danglingEdges.length} missing connections` : ''].filter(Boolean).join(' · ');
  warnings.replaceChildren(h('summary', {}, summary),
    h('ul', {}, ...messages.map((message) => h('li', {}, message))));
}

function createNodeRecord(node, source, onSelect, camera) {
  if (typeof onSelect !== 'function') throw new TypeError('Node selection needs a callback');
  if (!camera || typeof camera.reveal !== 'function') throw new TypeError('Nodes need a camera to reveal keyboard focus');
  const element = svgElement('g', { class: 'graph-node', role: 'button', tabindex: 0, 'data-node-id': node.id });
  const record = { element, node, source };
  element.addEventListener('click', () => onSelect(record.source));
  element.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter' && event.key !== ' ') return;
    event.preventDefault();
    event.stopPropagation();
    onSelect(record.source);
  });
  element.addEventListener('focus', () => camera.reveal(record.node));
  return record;
}

function updateNodes(layer, records, graph, sources, onSelect, camera) {
  const active = new Set(graph.nodes.map((node) => node.id));
  for (const [id, record] of records) {
    if (active.has(id)) continue;
    if (record.element === document.activeElement) layer.ownerSVGElement?.focus();
    record.element.remove();
    records.delete(id);
  }
  for (const node of graph.nodes) {
    if (!sources.has(node.id)) throw new Error('Each positioned node needs its source data');
    let record = records.get(node.id);
    if (!record) {
      record = createNodeRecord(node, sources.get(node.id), onSelect, camera);
      records.set(node.id, record);
      layer.append(record.element);
    }
    const previous = record.node;
    record.node = node;
    record.source = sources.get(node.id);
    paintNode(record.element, node);
    if (previous.x !== node.x || previous.y !== node.y) animateMove(record.element, [previous.x, previous.y], [node.x, node.y], 320);
  }
  if (records.size !== active.size) throw new Error('Graph node records must match the current layout');
}

function createView(label) {
  const zoomIn = h('button', { type: 'button', 'aria-label': 'Zoom in' }, '+');
  const zoomOut = h('button', { type: 'button', 'aria-label': 'Zoom out' }, '−');
  const fit = h('button', { type: 'button', 'aria-label': 'Fit entire graph' }, 'Fit');
  const output = h('output', { class: 'graph-zoom', 'aria-label': 'Zoom level', 'aria-live': 'off' }, '100%');
  const svg = svgElement('svg', { class: 'graph-viewport', role: 'group', tabindex: 0, 'aria-label': label,
    'aria-description': 'Tab to select nodes. Enter or Space opens a node. Arrow keys pan. Plus and minus zoom. Zero fits the graph.' });
  const world = svgElement('g', { class: 'graph-world' });
  const edges = svgElement('g', { class: 'graph-edges', role: 'group', 'aria-label': 'Connections' });
  const nodes = svgElement('g', { class: 'graph-nodes' });
  world.append(edges, nodes);
  svg.append(world);
  const warnings = h('details', { class: 'graph-warnings', hidden: true });
  const empty = h('p', { class: 'graph-empty', role: 'status' }, 'No graph nodes yet.');
  const help = h('p', { class: 'graph-help' }, 'Drag to pan · Scroll or pinch to zoom · Tab to nodes · Arrow keys pan');
  const element = h('section', { class: 'graph', 'aria-label': label },
    h('div', { class: 'graph-toolbar', role: 'group', 'aria-label': 'Graph camera controls' }, zoomOut, output, zoomIn, fit),
    svg, empty, warnings, help);
  return { element, svg, world, edges, nodes, warnings, empty, zoomIn, zoomOut, fit, output };
}

export function createGraph({ label, onSelect }) {
  if (typeof label !== 'string' || !label.trim()) throw new TypeError('Graph needs a nonempty accessible label');
  if (typeof onSelect !== 'function') throw new TypeError('Graph needs an onSelect callback');
  const direction = typeof matchMedia !== 'undefined' && matchMedia('(max-width: 699px)').matches ? 'vertical' : 'horizontal';
  const view = createView(label);
  const camera = createCameraController(view.svg, view.world, view.output);
  const records = new Map();
  let destroyed = false;
  const zoomIn = () => camera.zoom(1.2);
  const zoomOut = () => camera.zoom(1 / 1.2);
  view.zoomIn.addEventListener('click', zoomIn);
  view.zoomOut.addEventListener('click', zoomOut);
  view.fit.addEventListener('click', camera.fit);
  return {
    element: view.element,
    update: (data) => {
      if (destroyed) throw new Error('Cannot update a destroyed graph');
      const graph = layoutGraph(data, { direction });
      for (const node of graph.nodes) nodeDetails(node);
      const sources = new Map(data.nodes.map((node) => [node.id, node]));
      paintEdges(view.edges, graph);
      updateNodes(view.nodes, records, graph, sources, onSelect, camera);
      updateWarnings(view.warnings, graph);
      view.empty.hidden = graph.nodes.length !== 0;
      camera.update(graph);
    },
    destroy: () => {
      if (destroyed) return;
      destroyed = true;
      camera.destroy();
      view.zoomIn.removeEventListener('click', zoomIn);
      view.zoomOut.removeEventListener('click', zoomOut);
      view.fit.removeEventListener('click', camera.fit);
      records.clear();
      view.nodes.replaceChildren();
      view.edges.replaceChildren();
      view.element.remove();
    },
  };
}
