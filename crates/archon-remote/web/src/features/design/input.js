// Batches input events for design_input: at most 64 events per request, 60
// requests per second and two awaiting a reply. While they wait, consecutive
// moves keep only the latest and wheel ticks add up, so input never floods.
import { MAX_INPUT_EVENTS } from '../../core/design.js';

export const MAX_REQUESTS_PER_SECOND = 60;
export const MAX_OUTSTANDING = 2;
// Waiting this long on replies is shown, quietly: the preview is behind.
export const BUSY_AFTER_MS = 500;
const MAX_QUEUED = 512;
// Wheel ticks this close (app CSS px) scroll the same thing.
const WHEEL_MERGE_PX = 24;

const commutes = (e) => e.type === 'wheel' || (e.type === 'mouse' && e.action === 'move');
const round = (v) => Math.round(v * 10) / 10;

// `send(events)` may return a promise: the batch counts as outstanding until it settles.
export function createInputBatcher({
  send, now = () => performance.now(), setTimer = (fn, ms) => setTimeout(fn, ms), clearTimer = (t) => clearTimeout(t),
  perSecond = MAX_REQUESTS_PER_SECOND, maxEvents = MAX_INPUT_EVENTS, maxOutstanding = MAX_OUTSTANDING,
  onOverflow = () => {}, onBusy = () => {},
}) {
  if (typeof send !== 'function') throw new TypeError('the input batcher needs a send function');
  if (!(Number.isInteger(maxOutstanding) && maxOutstanding >= 1)) throw new RangeError(`at least one request may be outstanding, got ${maxOutstanding}`);
  const gap = 1000 / perSecond;
  let queue = [];
  let lastSent = -Infinity;
  let timer = null;
  const pressed = new Set();
  let overflowReported = false;
  let outstanding = 0;
  let epoch = 0; // a stop forgets replies still on their way
  let busy = false;
  let busyTimer = null;

  function setBusy(next) {
    if (busyTimer !== null && !next) { clearTimer(busyTimer); busyTimer = null; }
    if (busy === next) return;
    busy = next;
    onBusy(next);
  }

  // Blocked on replies: say so only if it lasts.
  function watchBusy() {
    if (busy || busyTimer !== null) return;
    busyTimer = setTimer(() => {
      busyTimer = null;
      if (queue.length > 0 && outstanding >= maxOutstanding) setBusy(true);
    }, BUSY_AFTER_MS);
  }

  function track(reply) {
    if (!reply || typeof reply.then !== 'function') return;
    const mine = epoch;
    outstanding += 1;
    const settled = () => {
      if (mine !== epoch) return;
      outstanding -= 1;
      console.assert(outstanding >= 0 && outstanding < maxOutstanding, 'Input replies outnumber their requests');
      if (queue.length > 0 && timer === null) timer = setTimer(flush, 0);
    };
    reply.then(settled, settled);
  }

  // Pointer moves and wheel ticks commute: a new one folds into the trailing
  // run of them. A move keeps only the latest; wheel ticks near each other add up.
  function merge(event) {
    const last = queue[queue.length - 1];
    if (last && event.type === 'touch' && last.type === 'touch' && event.action === 'move' && last.action === 'move') {
      queue[queue.length - 1] = event;
      return true;
    }
    if (!commutes(event)) return false;
    for (let i = queue.length - 1; i >= 0 && commutes(queue[i]); i -= 1) {
      const earlier = queue[i];
      if (event.type === 'mouse' && earlier.type === 'mouse' && earlier.button === event.button) {
        queue.splice(i, 1);
        queue.push(event);
        return true;
      }
      if (event.type === 'wheel' && earlier.type === 'wheel' && earlier.modifiers === event.modifiers
        && Math.hypot(earlier.x - event.x, earlier.y - event.y) <= WHEEL_MERGE_PX) {
        queue[i] = { ...event, dx: round(earlier.dx + event.dx), dy: round(earlier.dy + event.dy) };
        return true;
      }
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
    if (outstanding >= maxOutstanding) {
      watchBusy(); // the next reply flushes again
      return 0;
    }
    const wait = lastSent + gap - now();
    if (wait > 0) {
      timer = setTimer(flush, Math.ceil(wait));
      return 0;
    }
    const events = queue.slice(0, maxEvents);
    queue = queue.slice(maxEvents);
    if (queue.length === 0) overflowReported = false;
    lastSent = now();
    setBusy(false);
    track(send(events));
    if (queue.length > 0 && timer === null && outstanding < maxOutstanding) timer = setTimer(flush, Math.ceil(gap));
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
    get outstanding() { return outstanding; },
    stop() {
      if (timer !== null) clearTimer(timer);
      timer = null;
      queue = [];
      pressed.clear();
      overflowReported = false;
      epoch += 1;
      outstanding = 0;
      setBusy(false);
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
