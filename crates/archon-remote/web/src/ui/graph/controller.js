import { fitCamera, revealNode, zoomCamera } from './camera.js';
import { attachInteractions } from './interactions.js';

export function createCameraController(svg, world, output) {
  let bounds = null;
  let frame = null;
  let destroyed = false;
  let measured = false;
  let initialPending = true;
  let rect = { left: 0, top: 0 };
  const state = { camera: { x: 0, y: 0, scale: 1 }, viewport: { width: 320, height: 360 },
    local: (point) => ({ x: point.x - rect.left, y: point.y - rect.top }),
    center: () => ({ x: state.viewport.width / 2, y: state.viewport.height / 2 }),
    measure: () => { rect = svg.getBoundingClientRect(); } };
  const draw = () => {
    frame = null;
    world.setAttribute('transform', `translate(${state.camera.x} ${state.camera.y}) scale(${state.camera.scale})`);
    output.textContent = `${Math.round(state.camera.scale * 100)}%`;
  };
  const schedule = () => { if (frame === null && !destroyed) frame = requestAnimationFrame(draw); };
  const change = (camera) => { initialPending = false; state.camera = camera; schedule(); };
  const fit = () => { if (bounds) change(fitCamera(bounds, state.viewport)); };
  const resize = (entries) => {
    if (destroyed) return;
    const size = entries[0]?.contentRect;
    if (!size || size.width <= 0 || size.height <= 0) return;
    state.viewport = { width: size.width, height: size.height };
    measured = true;
    state.measure();
    if (initialPending && bounds) {
      state.camera = fitCamera(bounds, state.viewport, true);
      initialPending = false;
      schedule();
    }
  };
  const observer = new ResizeObserver(resize);
  observer.observe(svg);
  const detach = attachInteractions(svg, state, change, fit);
  return {
    fit,
    zoom: (factor) => change(zoomCamera(state.camera, factor, state.center())),
    reveal: (node) => change(revealNode(state.camera, node, state.viewport)),
    update: (graph) => {
      if (!graph.nodes.length) return;
      const first = bounds === null;
      bounds = { width: graph.width, height: graph.height };
      if (!first) return;
      state.camera = fitCamera(bounds, state.viewport, true);
      initialPending = !measured;
      schedule();
    },
    destroy: () => {
      destroyed = true;
      observer.disconnect();
      detach();
      if (frame !== null) cancelAnimationFrame(frame);
    },
  };
}
