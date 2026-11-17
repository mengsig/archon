import { BACKOFF, retryDelay } from './backoff.js';

// Detects a session whose device or role is no longer the one the app was built for.
export function deviceGuard(built, onChanged) {
  if (!built || typeof built.id !== 'string' || !built.id) throw new TypeError('the guard needs the device the app was built for');
  if (typeof onChanged !== 'function') throw new TypeError('the guard reports a changed device');
  let fired = false;
  return (device) => {
    if (fired) return false;
    const same = device?.id === built.id && device?.role === built.role;
    if (same) return false;
    fired = true;
    onChanged();
    return true;
  };
}

// A connection that keeps failing, another tab's pairing or a page shown again may
// mean the device changed; the session endpoint says so without a socket.
// `force` skips the gap; `cancel` keeps a late answer from reaching a stopped app.
// A forced check keeps retrying until the host answers; past `unsureAfter` failed
// reads `onUnsure(true)` withholds operator controls until an answer comes.
export function sessionChecker({ readSession, guard, stop, onUnpaired, onUnsure = () => {}, minGapMs, now = Date.now,
  unsureAfter = 4, limits = BACKOFF }) {
  assertCheckerArgs({ readSession, guard, stop, onUnpaired, onUnsure, minGapMs, unsureAfter });
  let lastCheck = -Infinity;
  let alive = true;
  let unsure = false;
  const retry = { timer: null, attempt: 0, round: 0 };

  function markUnsure(value) {
    if (unsure === value) return;
    unsure = value;
    onUnsure(value);
  }
  function scheduleRetry(round) {
    if (!alive || round !== retry.round) return;
    if (retry.attempt >= unsureAfter) markUnsure(true);
    const delay = retryDelay(retry.attempt, { limits });
    retry.attempt += 1;
    retry.timer = setTimeout(() => {
      retry.timer = null;
      read(round, true);
    }, delay);
  }
  function answered(current) {
    clearTimeout(retry.timer);
    retry.timer = null;
    retry.attempt = 0;
    markUnsure(false);
    if (current === null) {
      stop();
      onUnpaired('This device is no longer paired.');
    } else {
      guard(current.device);
    }
  }
  async function read(round, retrying) {
    // Unreachable, busy or timed out is no answer; only a definite 401 unpairs.
    const current = await readSession().catch(() => undefined);
    if (!alive) return;
    if (current === undefined) {
      if (retrying) scheduleRetry(round);
      return;
    }
    answered(current);
  }
  async function check({ force = false } = {}) {
    if (!alive || (!force && now() - lastCheck < minGapMs)) return;
    lastCheck = now();
    if (!force) return read(retry.round, false);
    // A fresh signal restarts the backoff and supersedes any retry still waiting.
    clearTimeout(retry.timer);
    retry.timer = null;
    retry.attempt = 0;
    retry.round += 1;
    return read(retry.round, true);
  }
  function cancel() {
    alive = false;
    clearTimeout(retry.timer);
    retry.timer = null;
  }
  return { check, cancel };
}

function assertCheckerArgs({ readSession, guard, stop, onUnpaired, onUnsure, minGapMs, unsureAfter }) {
  if (typeof readSession !== 'function' || typeof guard !== 'function') throw new TypeError('the session check needs a reader and a guard');
  if (typeof stop !== 'function' || typeof onUnpaired !== 'function') throw new TypeError('the session check needs stop and onUnpaired');
  if (typeof onUnsure !== 'function') throw new TypeError('onUnsure reports a session the app cannot confirm');
  if (!Number.isFinite(minGapMs) || minGapMs < 0) throw new RangeError(`minGapMs must be a wait, got ${minGapMs}`);
  if (!Number.isInteger(unsureAfter) || unsureAfter < 0) throw new RangeError(`unsureAfter must be a count, got ${unsureAfter}`);
}
