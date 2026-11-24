// Batches input events for design_input: at most 64 events per request and 60
// requests per second. Consecutive moves and wheel ticks are merged.
import { MAX_INPUT_EVENTS } from '../../core/design.js';

export const MAX_REQUESTS_PER_SECOND = 60;
const MAX_QUEUED = 512;

export function createInputBatcher({
  send, now = () => performance.now(), setTimer = (fn, ms) => setTimeout(fn, ms), clearTimer = (t) => clearTimeout(t),
  perSecond = MAX_REQUESTS_PER_SECOND, maxEvents = MAX_INPUT_EVENTS, onOverflow = () => {},
}) {
  if (typeof send !== 'function') throw new TypeError('the input batcher needs a send function');
  const gap = 1000 / perSecond;
  let queue = [];
  let lastSent = -Infinity;
  let timer = null;
  const pressed = new Set();
  let overflowReported = false;

  function merge(event) {
    const last = queue[queue.length - 1];
    if (last && event.type === 'mouse' && last.type === 'mouse' && event.action === 'move' && last.action === 'move' && last.button === event.button) {
      queue[queue.length - 1] = event;
      return true;
    }
    if (last && event.type === 'wheel' && last.type === 'wheel' && last.x === event.x && last.y === event.y && last.modifiers === event.modifiers) {
      queue[queue.length - 1] = { ...last, dx: last.dx + event.dx, dy: last.dy + event.dy };
      return true;
    }
    if (last && event.type === 'touch' && last.type === 'touch' && event.action === 'move' && last.action === 'move') {
      queue[queue.length - 1] = event;
      return true;
    }
    return false;
  }

  function admit(event) {
    const { type, action } = event;
    const key = type === 'key' ? `key:${event.code || event.key}`
      : type === 'mouse' && event.button !== 'none' ? `mouse:${event.button}`
      : type === 'touch' ? 'touch' : null;
    const down = key !== null && (action === 'down' || action === 'start');
    const up = key !== null && (action === 'up' || action === 'end' || action === 'cancel');
    const held = pressed.has(key);
    if (key !== null && (up || action === 'move') && !held) return false;
    if (merge(event)) return true;

    // Each admitted press reserves space for its release, even after it is sent.
    const reserve = down && !held ? 1 : up ? -1 : 0;
    while (queue.length + pressed.size + 1 + reserve > MAX_QUEUED) {
      const at = queue.findIndex((e) => e.type === 'wheel' || e.action === 'move');
      if (at !== -1) { queue.splice(at, 1); continue; }
      if (!overflowReported) { overflowReported = true; onOverflow(); }
      return false;
    }
    if (down) pressed.add(key);
    if (up) pressed.delete(key);
    queue.push(event);
    console.assert(queue.length + pressed.size <= MAX_QUEUED, 'Input queue exceeded its release reserve');
    console.assert(!up || !pressed.has(key), 'Input release did not clear its press');
    return true;
  }

  function flush() {
    if (timer !== null) clearTimer(timer);
    timer = null;
    if (queue.length === 0) return 0;
    const wait = lastSent + gap - now();
    if (wait > 0) {
      timer = setTimer(flush, Math.ceil(wait));
      return 0;
    }
    const events = queue.slice(0, maxEvents);
    queue = queue.slice(maxEvents);
    if (queue.length === 0) overflowReported = false;
    lastSent = now();
    send(events);
    if (queue.length > 0 && timer === null) timer = setTimer(flush, Math.ceil(gap));
    return events.length;
  }

  return {
    push(event) {
      const accepted = admit(event);
      if (accepted && timer === null) timer = setTimer(flush, 0);
      return accepted;
    },
    flush,
    get pending() { return queue.length; },
    stop() {
      if (timer !== null) clearTimer(timer);
      timer = null;
      queue = [];
      pressed.clear();
      overflowReported = false;
    },
  };
}

// CDP modifier bits.
export function modifiersOf(event) {
  return (event.altKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.metaKey ? 4 : 0) | (event.shiftKey ? 8 : 0);
}

const BUTTONS = ['left', 'middle', 'right'];
export function mouseEvent(action, point, { button = 0, clickCount = 0, modifiers = 0 } = {}) {
  return { type: 'mouse', action, x: point.x, y: point.y, button: action === 'move' && button === null ? 'none' : (BUTTONS[button] ?? 'none'), click_count: clickCount, modifiers };
}

export function wheelEvent(point, dx, dy, modifiers = 0) {
  return { type: 'wheel', x: point.x, y: point.y, dx, dy, modifiers };
}

export function touchEvent(action, points) {
  return { type: 'touch', action, points: points.map((p, i) => ({ x: p.x, y: p.y, id: p.id ?? i })) };
}

export function textEvent(text) {
  return { type: 'text', text };
}
