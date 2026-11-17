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
export function sessionChecker({ readSession, guard, stop, onUnpaired, minGapMs, now = Date.now }) {
  if (typeof readSession !== 'function' || typeof guard !== 'function') throw new TypeError('the session check needs a reader and a guard');
  if (typeof stop !== 'function' || typeof onUnpaired !== 'function') throw new TypeError('the session check needs stop and onUnpaired');
  if (!Number.isFinite(minGapMs) || minGapMs < 0) throw new RangeError(`minGapMs must be a wait, got ${minGapMs}`);
  let lastCheck = -Infinity;
  let alive = true;
  async function check({ force = false } = {}) {
    if (!alive || (!force && now() - lastCheck < minGapMs)) return;
    lastCheck = now();
    // Unreachable is expected while offline; only a definite 401 unpairs.
    const current = await readSession().catch(() => undefined);
    if (!alive) return;
    if (current === null) {
      stop();
      onUnpaired('This device is no longer paired.');
    } else if (current) {
      guard(current.device);
    }
  }
  return { check, cancel: () => { alive = false; } };
}
