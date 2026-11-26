// Frame scaling: the headless viewport is the preset's CSS size and the canvas
// shows it scaled. Every coordinate sent to the app is divided by the scale.
export const MIN_ZOOM = 1;
export const MAX_ZOOM = 4;

// mode: 'fit' (whole frame fits, never above 1:1), 'width' (phone: fit the width) or 'actual' (1:1).
export function fitScale({ frame, avail, mode = 'fit' }) {
  if (!(frame?.w > 0) || !(frame?.h > 0)) return 1;
  if (mode === 'actual') return 1;
  if (!(avail?.w > 0) || !(avail?.h > 0)) return 1;
  if (mode === 'width') return avail.w / frame.w;
  return Math.min(avail.w / frame.w, avail.h / frame.h, 1);
}

// The displayed scale, read from the canvas box itself (it includes local zoom).
export function effectiveScale(rect, frame) {
  return frame?.w > 0 && rect?.width > 0 ? rect.width / frame.w : 1;
}

// A pointer position in the page becomes a point in the app's CSS pixels.
export function toApp(clientX, clientY, rect, frame) {
  const scale = effectiveScale(rect, frame);
  const x = (clientX - rect.left) / scale;
  const y = (clientY - rect.top) / scale;
  return { x: clamp(round(x), 0, frame.w), y: clamp(round(y), 0, frame.h) };
}

// An app box becomes a box on the displayed canvas.
export function boxToScreen(box, scale) {
  return { x: box.x * scale, y: box.y * scale, w: box.w * scale, h: box.h * scale };
}

export function normalizeDrag(a, b, frame) {
  const x1 = clamp(Math.min(a.x, b.x), 0, frame.w);
  const y1 = clamp(Math.min(a.y, b.y), 0, frame.h);
  const x2 = clamp(Math.max(a.x, b.x), 0, frame.w);
  const y2 = clamp(Math.max(a.y, b.y), 0, frame.h);
  return { x: round(x1), y: round(y1), w: round(x2 - x1), h: round(y2 - y1) };
}

// Two fingers either pinch (local zoom, never forwarded) or drag together (the app scrolls).
export function classifyTwoFinger(prev, next, threshold = 4) {
  const spread = (pair) => Math.hypot(pair[0].x - pair[1].x, pair[0].y - pair[1].y);
  const centre = (pair) => ({ x: (pair[0].x + pair[1].x) / 2, y: (pair[0].y + pair[1].y) / 2 });
  const ds = spread(next) - spread(prev);
  const c0 = centre(prev);
  const c1 = centre(next);
  const dx = c1.x - c0.x;
  const dy = c1.y - c0.y;
  if (Math.abs(ds) > Math.hypot(dx, dy) && Math.abs(ds) > threshold) return { kind: 'zoom', ratio: spread(prev) > 0 ? spread(next) / spread(prev) : 1 };
  return { kind: 'pan', dx, dy };
}

// Chrome's line step; Firefox reports wheel notches in lines.
export const WHEEL_LINE_PX = 40;

// A wheel event's deltas in the app's CSS px. Pixel deltas are screen px on the
// scaled canvas (so content follows the fingers); lines and pages are the app's.
export function wheelDeltas(event, scale, frameHeight) {
  if (!(scale > 0) || !(frameHeight > 0)) throw new RangeError(`a wheel needs a positive scale and frame, got ${scale}, ${frameHeight}`);
  const unit = event.deltaMode === 1 ? WHEEL_LINE_PX : event.deltaMode === 2 ? frameHeight : 1 / scale;
  const dx = round((Number(event.deltaX) || 0) * unit);
  const dy = round((Number(event.deltaY) || 0) * unit);
  console.assert(Number.isFinite(dx) && Number.isFinite(dy), 'Wheel deltas are finite');
  return { dx, dy };
}

export function clampZoom(zoom) {
  return clamp(zoom, MIN_ZOOM, MAX_ZOOM);
}

export function clamp(value, lo, hi) {
  return Math.min(hi, Math.max(lo, value));
}

export function round(value) {
  return Math.round(value * 10) / 10;
}

// Keyboard path for a region: arrows move the box, Shift+arrows resize it.
export function nudgeBox(box, key, shift, frame, step = 10) {
  const d = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] }[key];
  if (!d) return box;
  if (shift) {
    const w = clamp(box.w + d[0] * step, 8, frame.w - box.x);
    const h = clamp(box.h + d[1] * step, 8, frame.h - box.y);
    return { ...box, w, h };
  }
  return { ...box, x: clamp(box.x + d[0] * step, 0, frame.w - box.w), y: clamp(box.y + d[1] * step, 0, frame.h - box.h) };
}

export function centredBox(frame) {
  const w = Math.round(frame.w / 3);
  const h = Math.round(frame.h / 4);
  return { x: Math.round((frame.w - w) / 2), y: Math.round((frame.h - h) / 2), w, h };
}
