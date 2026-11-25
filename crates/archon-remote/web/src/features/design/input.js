// Batches input events for design_input: at most 64 events per request, 60
// requests per second and one awaiting a reply, so archond runs them in order.
// While it waits, consecutive moves keep the latest and wheel ticks add up.
import { MAX_INPUT_EVENTS } from '../../core/design.js';

export const MAX_REQUESTS_PER_SECOND = 60;
export const MAX_OUTSTANDING = 1;
// Waiting this long on replies is shown, quietly: the preview is behind.
export const BUSY_AFTER_MS = 500;
const MAX_QUEUED = 512;
// A refused batch is tried again this often, in order, before it is given up.
export const RETRY_MS = 250;
export const MAX_RETRIES = 8;

const round = (v) => Math.round(v * 10) / 10;
// What a press holds down until its release, or null for events that hold nothing.
const heldKey = (e) => (e.type === 'key' ? `key:${e.code || e.key}`
  : e.type === 'mouse' && e.button !== 'none' ? `mouse:${e.button}`
    : e.type === 'touch' ? 'touch' : null);
const isPress = (e) => e.action === 'down' || e.action === 'start';
const isRelease = (e) => e.action === 'up' || e.action === 'end' || e.action === 'cancel';

// Releases in `events` whose press went out before them: dropping one would
// leave its key, button or finger down in Chrome.
export function strandedReleases(events) {
  const pressedHere = new Set();
  const stranded = [];
  for (const e of events) {
    const key = heldKey(e);
    if (key === null) continue;
    if (isPress(e)) pressedHere.add(key);
    else if (isRelease(e) && !pressedHere.delete(key)) stranded.push(e);
  }
  console.assert(stranded.length <= events.length, 'Stranded releases come from the batch');
  return stranded;
}

// `send(events)` may return a promise: the batch counts as outstanding until it
// settles. A rejection means archond refused it unrun; it goes again first.
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
  let retries = 0;

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

  function track(reply, events) {
    if (!reply || typeof reply.then !== 'function') return;
    const mine = epoch;
    outstanding += 1;
    const settled = (refused) => {
      if (mine !== epoch) return;
      outstanding -= 1;
      console.assert(outstanding >= 0 && outstanding < maxOutstanding, 'Input replies outnumber their requests');
      if (refused) return retry(events);
      retries = 0;
      if (queue.length === 0) setBusy(false); // caught up
      else if (timer === null) timer = setTimer(flush, 0);
    };
    reply.then(() => settled(false), () => settled(true));
  }

  // A refused batch goes first again, a few times; given up, it still keeps
  // the releases of presses already sent, so nothing stays held down.
  function retry(events) {
    console.assert(events.length > 0 && events.length <= maxEvents, 'Only a sent batch is retried');
    if (retries >= MAX_RETRIES) {
      retries = 0;
      setBusy(false);
      onOverflow();
      queue = strandedReleases(events).concat(queue);
    } else {
      retries += 1;
      queue = events.concat(queue);
      setBusy(true);
    }
    if (timer !== null) clearTimer(timer);
    timer = queue.length > 0 ? setTimer(flush, RETRY_MS) : null;
  }

  // Only back-to-back events merge, so order and targets stay as they were:
  // a move keeps the latest, wheel ticks at the same point add up.
  function merge(event) {
    const last = queue[queue.length - 1];
    if (!last || last.type !== event.type) return false;
    const moves = event.action === 'move' && last.action === 'move';
    if (event.type === 'touch' && moves) {
      queue[queue.length - 1] = event;
      return true;
    }
    if (event.type === 'mouse' && moves && last.button === event.button && last.modifiers === event.modifiers) {
      queue[queue.length - 1] = event;
      return true;
    }
    if (event.type === 'wheel' && last.modifiers === event.modifiers && last.x === event.x && last.y === event.y) {
      queue[queue.length - 1] = { ...event, dx: round(last.dx + event.dx), dy: round(last.dy + event.dy) };
      return true;
    }
    return false;
  }

  function admit(event) {
    const key = heldKey(event);
    const down = key !== null && isPress(event);
    const up = key !== null && isRelease(event);
    const held = pressed.has(key);
    if (key !== null && (up || event.action === 'move') && !held) return false;
    if (merge(event)) return true;

    // Each admitted press reserves space for its release, even after it is sent;
    // a refused batch put back may overrun it, so a release always gets in.
    const reserve = down && !held ? 1 : up ? -1 : 0;
    while (!up && queue.length + pressed.size + 1 + reserve > MAX_QUEUED) {
      const at = queue.findIndex((e) => e.type === 'wheel' || e.action === 'move');
      if (at !== -1) { queue.splice(at, 1); continue; }
      if (!overflowReported) { overflowReported = true; onOverflow(); }
      return false;
    }
    if (down) pressed.add(key);
    if (up) pressed.delete(key);
    queue.push(event);
    console.assert(queue.length + pressed.size <= MAX_QUEUED + maxEvents, 'Input queue exceeded its release reserve');
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
    track(send(events), events);
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
      retries = 0;
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
