// Batches input events for design_input: at most 64 events per request and 60
// requests per second. Consecutive moves and wheel ticks are merged.
import { MAX_INPUT_EVENTS } from '../../core/design.js';

export const MAX_REQUESTS_PER_SECOND = 60;
const MAX_QUEUED = 512;

export function createInputBatcher({
  send, now = () => performance.now(), setTimer = (fn, ms) => setTimeout(fn, ms), clearTimer = (t) => clearTimeout(t),
  perSecond = MAX_REQUESTS_PER_SECOND, maxEvents = MAX_INPUT_EVENTS,
}) {
  if (typeof send !== 'function') throw new TypeError('the input batcher needs a send function');
  const gap = 1000 / perSecond;
  let queue = [];
  let lastSent = -Infinity;
  let timer = null;

  function merge(event) {
    const last = queue[queue.length - 1];
    if (last && event.type === 'Mouse' && last.type === 'Mouse' && event.action === 'move' && last.action === 'move' && last.button === event.button) {
      queue[queue.length - 1] = event;
      return true;
    }
    if (last && event.type === 'Wheel' && last.type === 'Wheel' && last.x === event.x && last.y === event.y && last.modifiers === event.modifiers) {
      queue[queue.length - 1] = { ...last, dx: last.dx + event.dx, dy: last.dy + event.dy };
      return true;
    }
    if (last && event.type === 'Touch' && last.type === 'Touch' && event.action === 'move' && last.action === 'move') {
      queue[queue.length - 1] = event;
      return true;
    }
    return false;
  }

  function flush() {
    timer = null;
    if (queue.length === 0) return 0;
    const wait = lastSent + gap - now();
    if (wait > 0) {
      timer = setTimer(flush, Math.ceil(wait));
      return 0;
    }
    const events = queue.slice(0, maxEvents);
    queue = queue.slice(maxEvents);
    lastSent = now();
    send(events);
    if (queue.length > 0 && timer === null) timer = setTimer(flush, Math.ceil(gap));
    return events.length;
  }

  return {
    push(event) {
      if (!merge(event)) {
        queue.push(event);
        // Past the bound, the oldest pure moves go first; clicks and keys are never dropped.
        if (queue.length > MAX_QUEUED) {
          const at = queue.findIndex((e) => (e.type === 'Mouse' && e.action === 'move') || e.type === 'Wheel');
          queue.splice(at >= 0 ? at : 0, 1);
        }
      }
      if (timer === null) timer = setTimer(flush, 0);
    },
    flush,
    get pending() { return queue.length; },
    stop() {
      if (timer !== null) clearTimer(timer);
      timer = null;
      queue = [];
    },
  };
}

// CDP modifier bits.
export function modifiersOf(event) {
  return (event.altKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.metaKey ? 4 : 0) | (event.shiftKey ? 8 : 0);
}

const BUTTONS = ['left', 'middle', 'right'];
export function mouseEvent(action, point, { button = 0, clickCount = 0, modifiers = 0 } = {}) {
  return { type: 'Mouse', action, x: point.x, y: point.y, button: action === 'move' && button === null ? 'none' : (BUTTONS[button] ?? 'none'), click_count: clickCount, modifiers };
}

export function wheelEvent(point, dx, dy, modifiers = 0) {
  return { type: 'Wheel', x: point.x, y: point.y, dx, dy, modifiers };
}

export function touchEvent(action, points) {
  return { type: 'Touch', action, points: points.map((p, i) => ({ x: p.x, y: p.y, id: p.id ?? i })) };
}

export function textEvent(text) {
  return { type: 'Text', text };
}
