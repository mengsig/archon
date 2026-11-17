const MIN_SCALE = 0.001;
const MAX_SCALE = 3;
// A readable fit zooms out this far at most; a graph needing more starts at
// 100% and pans, so its text stays legible.
export const READABLE_SCALE = 0.5;

function positiveSize(size) {
  if (!size || !Number.isFinite(size.width) || !Number.isFinite(size.height)) {
    throw new TypeError('Camera dimensions must be finite');
  }
  if (size.width <= 0 || size.height <= 0) throw new RangeError('Camera dimensions must be positive');
}

function validCamera(camera) {
  if (!camera || ![camera.x, camera.y, camera.scale].every(Number.isFinite)) {
    throw new TypeError('Camera coordinates must be finite');
  }
  if (camera.scale < MIN_SCALE || camera.scale > MAX_SCALE) throw new RangeError('Camera scale is out of bounds');
}

export function fitCamera(bounds, viewport, readable = false) {
  positiveSize(bounds);
  positiveSize(viewport);
  const fit = Math.min(viewport.width / bounds.width, viewport.height / bounds.height, 1);
  const scale = readable ? (fit >= READABLE_SCALE ? fit : 1) : Math.max(MIN_SCALE, fit);
  return { scale, x: Math.max(0, (viewport.width - bounds.width * scale) / 2),
    y: Math.max(0, (viewport.height - bounds.height * scale) / 2) };
}

export function zoomCamera(camera, factor, anchor) {
  validCamera(camera);
  if (!Number.isFinite(factor) || factor <= 0) throw new RangeError('Zoom factor must be finite and positive');
  if (!anchor || ![anchor.x, anchor.y].every(Number.isFinite)) throw new TypeError('Zoom anchor must be finite');
  const scale = Math.max(MIN_SCALE, Math.min(MAX_SCALE, camera.scale * factor));
  const ratio = scale / camera.scale;
  const result = { scale, x: anchor.x - (anchor.x - camera.x) * ratio, y: anchor.y - (anchor.y - camera.y) * ratio };
  validCamera(result);
  return result;
}

export function panCamera(camera, dx, dy) {
  validCamera(camera);
  if (![dx, dy].every(Number.isFinite)) throw new TypeError('Pan distance must be finite');
  const x = camera.x + dx;
  const y = camera.y + dy;
  if (![x, y].every(Number.isFinite)) throw new RangeError('Pan exceeds finite coordinates');
  return { ...camera, x, y };
}

export function revealNode(camera, node, viewport) {
  validCamera(camera);
  positiveSize(viewport);
  if (!node || ![node.x, node.y, node.width, node.height].every(Number.isFinite)) {
    throw new TypeError('Revealed node bounds must be finite');
  }
  if (node.width <= 0 || node.height <= 0) throw new RangeError('Revealed node dimensions must be positive');
  const left = node.x * camera.scale + camera.x;
  const top = node.y * camera.scale + camera.y;
  const right = left + node.width * camera.scale;
  const bottom = top + node.height * camera.scale;
  const dx = left < 12 ? 12 - left : Math.min(0, viewport.width - 12 - right);
  const dy = top < 12 ? 12 - top : Math.min(0, viewport.height - 12 - bottom);
  return panCamera(camera, dx, dy);
}
