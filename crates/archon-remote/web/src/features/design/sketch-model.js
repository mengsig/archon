// A sketch as data: strokes, rectangles, arrows and text on a frozen frame or a
// blank screen. serialise() is what design_upload carries in `sketch`.
export const MAX_SKETCH_ITEMS = 2000;
export const MAX_SKETCH_BYTES = 200 * 1024;
export const TOOLS = ['pen', 'rect', 'arrow', 'text'];

export function createSketch({ w, h }) {
  let items = [];
  return {
    get items() { return items; },
    get size() { return { w, h }; },
    add(item) {
      if (!TOOLS.includes(item?.tool)) throw new RangeError(`no sketch tool named ${item?.tool}`);
      if (items.length >= MAX_SKETCH_ITEMS) return false;
      items = [...items, clean(item)];
      return true;
    },
    undo() { items = items.slice(0, -1); },
    clear() { items = []; },
    serialise: () => serialise(items, { w, h }),
  };
}

const r1 = (n) => Math.round(n * 10) / 10;

function clean(item) {
  switch (item.tool) {
    case 'pen': return { tool: 'pen', points: item.points.map(([x, y]) => [r1(x), r1(y)]) };
    case 'rect': return { tool: 'rect', x: r1(item.x), y: r1(item.y), w: r1(item.w), h: r1(item.h) };
    case 'arrow': return { tool: 'arrow', x1: r1(item.x1), y1: r1(item.y1), x2: r1(item.x2), y2: r1(item.y2) };
    default: return { tool: 'text', x: r1(item.x), y: r1(item.y), text: String(item.text).slice(0, 200) };
  }
}

export function serialise(items, size) {
  let kept = items.slice(0, MAX_SKETCH_ITEMS);
  let json = JSON.stringify({ v: 1, w: size.w, h: size.h, items: kept });
  // Over the cap, the oldest strokes go first so the newest marks survive.
  while (json.length > MAX_SKETCH_BYTES && kept.length > 1) {
    kept = kept.slice(Math.ceil(kept.length / 10));
    json = JSON.stringify({ v: 1, w: size.w, h: size.h, items: kept });
  }
  return json;
}

// Largest size within the side cap, keeping the shape.
export function fitWithin(w, h, maxSide) {
  const scale = Math.min(1, maxSide / Math.max(w, h));
  return { w: Math.max(1, Math.round(w * scale)), h: Math.max(1, Math.round(h * scale)) };
}

export const UPLOAD_TYPES = ['image/png', 'image/jpeg', 'image/webp'];
