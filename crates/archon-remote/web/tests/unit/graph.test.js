import test from 'node:test';
import assert from 'node:assert/strict';
import { installDom, Element } from '../support/dom-shim.js';
import { layoutGraph, CARD_WIDTH, CARD_HEIGHT } from '../../src/ui/graph/layout.js';
import { fitCamera, zoomCamera, panCamera, revealNode, READABLE_SCALE } from '../../src/ui/graph/camera.js';
import { createGraph } from '../../src/ui/graph/graph.js';

const node = (id, extra = {}) => ({ id, title: `Task ${id}`, subtitle: 'Work item', state: 'running', ...extra });
const edge = (from, to, extra = {}) => ({ from, to, ...extra });
const layout = (ids, edges = []) => layoutGraph({ nodes: ids.map((id) => node(id)), edges });

function assertPositions(graph) {
  const occupied = new Set();
  for (const item of graph.nodes) {
    assert.ok([item.x, item.y, item.width, item.height].every(Number.isFinite));
    assert.equal(item.width, CARD_WIDTH);
    assert.equal(item.height, CARD_HEIGHT);
    const position = `${item.x}:${item.y}`;
    assert.ok(!occupied.has(position), `overlapping card ${item.id}`);
    occupied.add(position);
    assert.ok(item.x >= 0 && item.y >= 0);
    assert.ok(item.x + item.width < graph.width && item.y + item.height < graph.height);
  }
}

test('a chain advances through deterministic layers without changing input', () => {
  const input = { nodes: ['a', 'b', 'c'].map((id) => Object.freeze(node(id))),
    edges: Object.freeze([Object.freeze(edge('a', 'b')), Object.freeze(edge('b', 'c'))]) };
  const graph = layoutGraph(input);
  assert.deepEqual(graph.nodes.map((item) => item.layer), [0, 1, 2]);
  assert.deepEqual(graph, layoutGraph(input));
  assert.equal(input.nodes[0].x, undefined);
  assert.deepEqual(graph.cycleIds, []);
  assertPositions(graph);
});

test('fan-out and disconnected forests keep siblings separate', () => {
  const graph = layout(['a', 'b', 'c', 'd', 'e', 'f'], [edge('a', 'b'), edge('a', 'c'), edge('d', 'e')]);
  assert.deepEqual(graph.nodes.map((item) => item.layer), [0, 1, 1, 0, 1, 0]);
  assertPositions(graph);
  assert.equal(graph.nodes[1].x, graph.nodes[2].x);
  assert.notEqual(graph.nodes[1].y, graph.nodes[2].y);
});

test('a diamond waits for the deepest prerequisite and tolerates repeated edges', () => {
  const graph = layout(['a', 'b', 'c', 'd'], [edge('a', 'b'), edge('a', 'c'), edge('b', 'c'),
    edge('c', 'd'), edge('c', 'd')]);
  assert.deepEqual(graph.nodes.map((item) => item.layer), [0, 1, 2, 3]);
  assert.equal(graph.edges.length, 5);
  assertPositions(graph);
});

test('cycles and self-loops are explicit while their downstream nodes remain acyclic', () => {
  const graph = layout(['a', 'b', 'c', 'd'], [edge('a', 'b'), edge('b', 'a'), edge('b', 'c'), edge('d', 'd')]);
  assert.deepEqual(graph.cycleIds, ['a', 'b', 'd']);
  assert.deepEqual(graph.nodes.map((item) => item.layer), [0, 0, 1, 0]);
  assertPositions(graph);
});

test('missing endpoints are retained as diagnostics and excluded from layout edges', () => {
  const graph = layout(['a', 'b'], [edge('a', 'b'), edge('a', 'missing'), edge('gone', 'a')]);
  assert.equal(graph.edges.length, 1);
  assert.deepEqual(graph.danglingEdges.map((item) => [item.from, item.to]), [['a', 'missing'], ['gone', 'a']]);
  assertPositions(graph);
});

test('500 nodes in a chain and a cycle do not recurse or lose nodes', () => {
  const ids = Array.from({ length: 500 }, (_, index) => String(index));
  const edges = ids.slice(1).map((id, index) => edge(String(index), id));
  const chain = layout(ids, edges);
  assert.equal(chain.nodes.at(-1).layer, 499);
  assertPositions(chain);
  const cycle = layout(ids, [...edges, edge('499', '0')]);
  assert.equal(cycle.cycleIds.length, 500);
  assert.ok(cycle.nodes.every((item) => item.layer === 0));
  assertPositions(cycle);
});

test('empty graphs have finite bounds and special ids stay ordinary map keys', () => {
  assert.deepEqual(layout([], []), { nodes: [], edges: [], danglingEdges: [], direction: 'horizontal', cycleIds: [], width: 48, height: 48 });
  const graph = layout(['__proto__', 'constructor', '<script>'], [edge('__proto__', 'constructor')]);
  assert.equal(graph.nodes.length, 3);
  assertPositions(graph);
});

test('invalid graph shapes, ids, labels and edge values fail at the boundary', () => {
  for (const input of [{ nodes: null, edges: [] }, { nodes: [], edges: null },
    { nodes: [null], edges: [] }, { nodes: [node('')], edges: [] },
    { nodes: [node('a'), node('a')], edges: [] }, { nodes: [node('a', { title: 3 })], edges: [] },
    { nodes: [node('a', { state: {} })], edges: [] }, { nodes: [], edges: [null] },
    { nodes: [], edges: [edge('', 'b')] }, { nodes: [], edges: [edge('a', 'b', { kind: {} })] },
    { nodes: [], edges: [edge('a', 'b', { satisfied: 'yes' })] }]) {
    assert.throws(() => layoutGraph(input), TypeError);
  }
});

test('initial fitting preserves phone readability and explicit fitting shows the entire graph', () => {
  const bounds = { width: 3000, height: 2000 };
  const viewport = { width: 360, height: 480 };
  const initial = fitCamera(bounds, viewport, true);
  const fit = fitCamera(bounds, viewport);
  assert.equal(initial.scale, READABLE_SCALE);
  assert.equal(initial.x, 0);
  assert.ok(fit.scale < initial.scale);
  assert.ok(bounds.width * fit.scale <= viewport.width);
  assert.ok(bounds.height * fit.scale <= viewport.height);
});

test('initial fitting shows a desktop work graph whole once it fits a readable scale', () => {
  const bounds = { width: 1648, height: 432 };
  const viewport = { width: 990, height: 464 };
  const initial = fitCamera(bounds, viewport, true);
  assert.ok(initial.scale >= READABLE_SCALE && initial.scale < 1, `scale ${initial.scale}`);
  assert.ok(bounds.width * initial.scale <= viewport.width);
  assert.ok(bounds.height * initial.scale + initial.y <= viewport.height);
});

test('zoom preserves the world point under its anchor and clamps extreme factors', () => {
  const camera = { x: 20, y: -40, scale: 0.8 };
  const anchor = { x: 120, y: 80 };
  const zoomed = zoomCamera(camera, 2, anchor);
  assert.equal((anchor.x - camera.x) / camera.scale, (anchor.x - zoomed.x) / zoomed.scale);
  assert.equal((anchor.y - camera.y) / camera.scale, (anchor.y - zoomed.y) / zoomed.scale);
  assert.equal(zoomCamera(camera, 1e30, anchor).scale, 3);
  assert.equal(zoomCamera(camera, 1e-30, anchor).scale, 0.001);
  assert.deepEqual(panCamera(camera, 10, 20), { x: 30, y: -20, scale: 0.8 });
});

test('focus reveals offscreen cards without changing zoom', () => {
  const camera = { x: 0, y: 0, scale: 1 };
  const revealed = revealNode(camera, { x: 500, y: 600, width: 240, height: 88 }, { width: 360, height: 400 });
  assert.equal(revealed.scale, 1);
  assert.equal(revealed.x + 500 + 240, 348);
  assert.equal(revealed.y + 600 + 88, 388);
});

test('camera rejects invalid dimensions, coordinates, zoom factors, and overflow', () => {
  assert.throws(() => fitCamera({ width: 0, height: 2 }, { width: 1, height: 2 }), RangeError);
  assert.throws(() => fitCamera({ width: 1, height: 2 }, { width: NaN, height: 2 }), TypeError);
  assert.throws(() => zoomCamera({ x: Infinity, y: 0, scale: 1 }, 2, { x: 0, y: 0 }), TypeError);
  assert.throws(() => zoomCamera({ x: 0, y: 0, scale: 1 }, 0, { x: 0, y: 0 }), RangeError);
  assert.throws(() => zoomCamera({ x: 0, y: 0, scale: 1 }, 2, { x: NaN, y: 0 }), TypeError);
  assert.throws(() => panCamera({ x: 0, y: 0, scale: 8 }, 0, 0), RangeError);
  assert.throws(() => panCamera({ x: 1e308, y: 0, scale: 1 }, 1e308, 0), RangeError);
  assert.throws(() => zoomCamera({ x: 1e308, y: 0, scale: 1 }, 3, { x: 0, y: 0 }), TypeError);
  assert.throws(() => revealNode({ x: 0, y: 0, scale: 1 }, { x: 0, y: 0, width: -1, height: 1 },
    { width: 360, height: 400 }), RangeError);
});

class GraphElement extends Element {
  handlers = new Map();
  captures = new Set();
  addEventListener(type, handler, options) {
    const list = this.handlers.get(type) ?? [];
    list.push({ handler, capture: options === true });
    this.handlers.set(type, list);
  }
  removeEventListener(type, handler) {
    this.handlers.set(type, (this.handlers.get(type) ?? []).filter((entry) => entry.handler !== handler));
  }
  append(...nodes) {
    for (const item of nodes) {
      if (item.tag === '#fragment') { this.append(...item.children); continue; }
      item.parent = this;
      this.children.push(item);
    }
  }
  remove() {
    if (!this.parent) return;
    this.parent.children = this.parent.children.filter((item) => item !== this);
    this.parent = null;
  }
  closest(selector) {
    if (selector === '.graph-node' && this.className.split(' ').includes('graph-node')) return this;
    return this.parent?.closest(selector) ?? null;
  }
  focus() { document.activeElement = this; this.dispatch('focus', { bubbles: false }); }
  get ownerSVGElement() { return this.tag === 'svg' ? this : this.parent?.ownerSVGElement; }
  setPointerCapture(id) { this.captures.add(id); }
  hasPointerCapture(id) { return this.captures.has(id); }
  releasePointerCapture(id) { this.captures.delete(id); }
  getBoundingClientRect() { this.reads = (this.reads ?? 0) + 1; return { left: 10, top: 20, width: 360, height: 400 }; }
  dispatch(type, properties = {}) {
    const event = { type, target: this, bubbles: true, prevented: false, stopped: false,
      preventDefault() { this.prevented = true; }, stopPropagation() { this.stopped = true; },
      stopImmediatePropagation() { this.stopped = true; this.immediate = true; }, ...properties };
    const path = [this];
    while (path.at(-1).parent) path.push(path.at(-1).parent);
    for (const capture of [true, false]) {
      for (const item of capture ? [...path].reverse() : path) {
        for (const entry of item.handlers?.get(type) ?? []) {
          if (entry.capture === capture) entry.handler(event);
          if (event.immediate) break;
        }
        if (event.stopped || (!capture && !event.bubbles)) break;
      }
      if (event.stopped) break;
    }
    return event;
  }
}

function domHarness() {
  installDom();
  document.createElement = (tag) => new GraphElement(tag);
  document.createElementNS = (_ns, tag) => new GraphElement(tag);
  document.createDocumentFragment = () => new GraphElement('#fragment');
  const observers = [];
  const frames = new Map();
  let nextFrame = 0;
  globalThis.ResizeObserver = class {
    constructor(callback) { this.callback = callback; observers.push(this); }
    observe(element) { this.element = element; }
    disconnect() { this.disconnected = true; }
  };
  globalThis.requestAnimationFrame = (callback) => { frames.set(++nextFrame, callback); return nextFrame; };
  globalThis.cancelAnimationFrame = (id) => frames.delete(id);
  return { observers, frames, flush: () => {
    const pending = [...frames.values()];
    frames.clear();
    for (const callback of pending) callback();
  }, resize: () => {
    for (const observer of observers) observer.callback([{ contentRect: { width: 360, height: 400 } }]);
  } };
}

function graphParts(graph) {
  const groups = graph.element.query('g');
  return { svg: graph.element.query('svg')[0],
    world: groups.find((item) => item.className === 'graph-world'),
    nodes: groups.filter((item) => item.className === 'graph-node'),
    warnings: graph.element.query('details')[0], buttons: graph.element.query('button') };
}

const sample = () => ({ nodes: [node('a'), node('b')], edges: [edge('a', 'b', { kind: 'stacked_on', satisfied: false })] });

test('renderer uses plain text, accessible nodes and native labelled controls', () => {
  const harness = domHarness();
  const selected = [];
  const graph = createGraph({ label: 'Dependencies', onSelect: (value) => selected.push(value) });
  const input = sample();
  input.nodes[0].title = '<script>alert("x")</script>';
  input.nodes[0].meta = ['A very long full metadata value', 3];
  graph.update(input);
  harness.resize();
  harness.flush();
  const parts = graphParts(graph);
  assert.equal(parts.svg.getAttribute('aria-label'), 'Dependencies');
  assert.equal(parts.nodes[0].getAttribute('role'), 'button');
  assert.equal(parts.nodes[0].getAttribute('tabindex'), '0');
  assert.match(parts.nodes[0].getAttribute('aria-label'), /<script>.*A very long full metadata value/);
  assert.equal(graph.element.query('script').length, 0);
  const connection = graph.element.query('g').find((item) => item.className === 'graph-edge');
  assert.match(connection.getAttribute('aria-label'), /stacked_on · unsatisfied/);
  assert.equal(connection.getAttribute('role'), 'img');
  assert.equal(parts.buttons.length, 3);
  assert.deepEqual(parts.buttons.map((item) => item.getAttribute('aria-label')), ['Zoom out', 'Zoom in', 'Fit entire graph']);
  parts.nodes[0].dispatch('click');
  assert.equal(selected[0], input.nodes[0]);
  for (const key of ['Enter', ' ']) assert.ok(parts.nodes[1].dispatch('keydown', { key }).prevented);
  assert.equal(selected.length, 3);
  assert.equal(selected[2], input.nodes[1]);
  for (const item of [graph.element, ...graph.element.query('g'), ...graph.element.query('text')]) {
    assert.equal(item.getAttribute('style'), null);
  }
  graph.destroy();
});

test('live updates preserve focused node elements, source data and the camera', () => {
  const harness = domHarness();
  const selected = [];
  const graph = createGraph({ label: 'Tasks', onSelect: (value) => selected.push(value) });
  graph.update(sample());
  harness.resize();
  harness.flush();
  const parts = graphParts(graph);
  parts.nodes[0].focus();
  parts.svg.dispatch('keydown', { key: 'ArrowRight' });
  harness.flush();
  const before = parts.world.getAttribute('transform');
  const next = sample();
  next.nodes[0].title = 'Updated task';
  next.nodes.push(node('c'));
  graph.update(next);
  harness.flush();
  assert.equal(graphParts(graph).nodes[0], parts.nodes[0]);
  assert.equal(document.activeElement, parts.nodes[0]);
  assert.equal(parts.world.getAttribute('transform'), before);
  parts.nodes[0].dispatch('click');
  assert.equal(selected[0], next.nodes[0]);
  assert.match(parts.nodes[0].getAttribute('aria-label'), /Updated task/);
  graph.destroy();
});

test('warnings visibly explain cycles and dangling edges and empty state clears on data', () => {
  domHarness();
  const graph = createGraph({ label: 'Warnings', onSelect() {} });
  graph.update({ nodes: [], edges: [] });
  assert.equal(graph.element.query('p')[0].hidden, false);
  graph.update({ nodes: [node('a')], edges: [edge('a', 'a'), edge('a', 'absent')] });
  const parts = graphParts(graph);
  assert.equal(parts.warnings.hidden, false);
  assert.match(parts.warnings.textContent, /Dependency cycle: a/);
  assert.match(parts.warnings.textContent, /Missing endpoint: a → absent/);
  assert.match(parts.nodes[0].getAttribute('aria-label'), /Dependency cycle/);
  assert.equal(graph.element.query('p')[0].hidden, true);
  graph.destroy();
});

test('wheel and keyboard camera events coalesce into one frame without frame layout reads', () => {
  const harness = domHarness();
  const graph = createGraph({ label: 'Camera', onSelect() {} });
  graph.update(sample());
  harness.resize();
  harness.flush();
  const { svg, world, buttons } = graphParts(graph);
  const before = world.getAttribute('transform');
  const reads = svg.reads;
  for (let index = 0; index < 10; index++) {
    const event = svg.dispatch('wheel', { deltaY: -10, deltaMode: 0, clientX: 190, clientY: 220 });
    assert.ok(event.prevented);
  }
  assert.equal(harness.frames.size, 1);
  assert.equal(world.getAttribute('transform'), before);
  harness.flush();
  assert.notEqual(world.getAttribute('transform'), before);
  assert.equal(svg.reads, reads);
  for (const key of ['ArrowLeft', 'ArrowDown', '+', '-', '0']) assert.ok(svg.dispatch('keydown', { key }).prevented);
  assert.equal(svg.dispatch('keydown', { key: 'Tab' }).prevented, false);
  assert.equal(svg.dispatch('keydown', { key: 'ArrowRight', ctrlKey: true }).prevented, false);
  for (const button of buttons) button.dispatch('click');
  assert.equal(harness.frames.size, 1);
  graph.destroy();
  assert.equal(harness.frames.size, 0);
  assert.equal(harness.observers[0].disconnected, true);
  assert.equal(svg.handlers.get('wheel').length, 0);
  assert.throws(() => graph.update(sample()), /destroyed/);
  graph.destroy();
});

test('touch pan and pinch retain node click targets and suppress selection after dragging', () => {
  const harness = domHarness();
  const selected = [];
  const graph = createGraph({ label: 'Gestures', onSelect: (value) => selected.push(value) });
  graph.update(sample());
  harness.resize();
  harness.flush();
  const { svg, world, nodes } = graphParts(graph);
  const down = (id, x, target = svg) => target.dispatch('pointerdown', {
    pointerId: id, pointerType: 'touch', clientX: x, clientY: 100, button: 0 });
  down(1, 100, nodes[0]);
  assert.equal(nodes[0].hasPointerCapture(1), true);
  assert.equal(svg.hasPointerCapture(1), false);
  svg.dispatch('pointerup', { pointerId: 1 });
  nodes[0].dispatch('click', { detail: 1 });
  assert.equal(selected.length, 1);
  const before = world.getAttribute('transform');
  down(2, 100);
  svg.dispatch('pointermove', { pointerId: 2, clientX: 120, clientY: 110 });
  down(3, 200);
  svg.dispatch('pointermove', { pointerId: 3, clientX: 240, clientY: 100 });
  harness.flush();
  assert.notEqual(world.getAttribute('transform'), before);
  svg.dispatch('pointerup', { pointerId: 2 });
  svg.dispatch('pointerup', { pointerId: 3 });
  assert.equal(nodes[0].dispatch('click', { detail: 1 }).prevented, true);
  assert.equal(selected.length, 1);
  graph.destroy();
});

test('invalid metadata fails before changing the current visible graph', () => {
  domHarness();
  assert.throws(() => createGraph({ label: '', onSelect() {} }), TypeError);
  assert.throws(() => createGraph({ label: 'Tasks' }), TypeError);
  const graph = createGraph({ label: 'Tasks', onSelect() {} });
  graph.update(sample());
  const before = graphParts(graph).nodes[0].getAttribute('aria-label');
  assert.throws(() => graph.update({ nodes: [node('a', { meta: { unexpected: 'object' } })], edges: [] }), TypeError);
  const circular = [];
  circular.push(circular);
  assert.throws(() => graph.update({ nodes: [node('a', { meta: circular })], edges: [] }), TypeError);
  assert.equal(graphParts(graph).nodes[0].getAttribute('aria-label'), before);
  graph.destroy();
});

test('phone layout runs down the page and keeps fan-out siblings separate', () => {
  const graph = layoutGraph({ nodes: ['a', 'b', 'c', 'd'].map((id) => node(id)),
    edges: [edge('a', 'b'), edge('a', 'c'), edge('b', 'd')] }, { direction: 'vertical' });
  assert.equal(graph.direction, 'vertical');
  assert.equal(graph.nodes[1].y, graph.nodes[2].y);
  assert.notEqual(graph.nodes[1].x, graph.nodes[2].x);
  assert.ok(graph.nodes[3].y > graph.nodes[1].y);
  assertPositions(graph);
});
