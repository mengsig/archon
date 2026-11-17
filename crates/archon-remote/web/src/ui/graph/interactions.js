import { panCamera, zoomCamera } from './camera.js';

function center(points) {
  const values = [...points.values()];
  if (!values.length) throw new Error('A gesture needs a pointer');
  if (values.length > 2) throw new Error('A gesture uses at most two pointers');
  const first = values[0];
  const second = values[1] ?? first;
  return { x: (first.x + second.x) / 2, y: (first.y + second.y) / 2,
    distance: Math.hypot(second.x - first.x, second.y - first.y) };
}

function pointerHandlers(svg, state, change, listen) {
  const points = new Map();
  let previous = null;
  let origin = null;
  let dragged = false;
  const end = (event) => {
    const point = points.get(event.pointerId);
    points.delete(event.pointerId);
    previous = points.size ? center(points) : null;
    if (point?.target.hasPointerCapture(event.pointerId)) point.target.releasePointerCapture(event.pointerId);
  };
  listen(svg, 'pointerdown', (event) => {
    if ((event.pointerType === 'mouse' && event.button !== 0) || points.size === 2) return;
    state.measure();
    const target = event.target.closest('.graph-node') ?? svg;
    points.set(event.pointerId, { x: event.clientX, y: event.clientY, target });
    previous = center(points);
    origin = previous;
    dragged = points.size > 1;
    target.setPointerCapture(event.pointerId);
  });
  listen(svg, 'pointermove', (event) => {
    if (!points.has(event.pointerId)) return;
    points.set(event.pointerId, { ...points.get(event.pointerId), x: event.clientX, y: event.clientY });
    const next = center(points);
    dragged ||= Math.hypot(next.x - origin.x, next.y - origin.y) > 5;
    let camera = panCamera(state.camera, next.x - previous.x, next.y - previous.y);
    if (points.size === 2 && previous.distance > 0 && next.distance > 0) {
      camera = zoomCamera(camera, next.distance / previous.distance, state.local(next));
    }
    previous = next;
    change(camera);
  });
  listen(svg, 'pointerup', end);
  listen(svg, 'pointercancel', end);
  listen(svg, 'lostpointercapture', (event) => {
    points.delete(event.pointerId);
    previous = points.size ? center(points) : null;
  });
  listen(svg, 'click', (event) => {
    if (dragged && event.detail !== 0) { event.preventDefault(); event.stopImmediatePropagation(); }
    dragged = false;
  }, true);
  return () => {
    for (const [id, point] of points) if (point.target.hasPointerCapture(id)) point.target.releasePointerCapture(id);
    points.clear();
  };
}

function keyboardHandler(event, state, change, fit) {
  if (event.altKey || event.ctrlKey || event.metaKey) return;
  const direction = { ArrowLeft: [48, 0], ArrowRight: [-48, 0], ArrowUp: [0, 48], ArrowDown: [0, -48] }[event.key];
  if (direction) change(panCamera(state.camera, ...direction));
  else if (event.key === '+' || event.key === '=' || event.key === '-') {
    change(zoomCamera(state.camera, event.key === '-' ? 1 / 1.2 : 1.2, state.center()));
  } else if (event.key === '0') fit();
  else return;
  event.preventDefault();
}

export function attachInteractions(svg, state, change, fit) {
  if (typeof change !== 'function' || typeof fit !== 'function') throw new TypeError('Camera controls need callbacks');
  if (!svg || typeof svg.addEventListener !== 'function') throw new TypeError('Camera controls need an SVG element');
  const cleanups = [];
  const listen = (element, type, handler, options) => {
    element.addEventListener(type, handler, options);
    cleanups.push(() => element.removeEventListener(type, handler, options));
  };
  const releasePointers = pointerHandlers(svg, state, change, listen);
  listen(svg, 'keydown', (event) => keyboardHandler(event, state, change, fit));
  listen(svg, 'pointerenter', state.measure);
  listen(svg, 'wheel', (event) => {
    event.preventDefault();
    const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? state.viewport.height : 1;
    const factor = Math.exp(Math.max(-1, Math.min(1, -event.deltaY * unit * 0.002)));
    change(zoomCamera(state.camera, factor, state.local({ x: event.clientX, y: event.clientY })));
  }, { passive: false });
  return () => {
    for (const cleanup of cleanups) cleanup();
    releasePointers();
  };
}
