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

// A connection that keeps failing may mean the device was revoked or re-paired while
// the socket was down; the session endpoint says so without a socket.
export function sessionChecker({ readSession, guard, stop, onUnpaired, minGapMs, now = Date.now }) {
  if (typeof readSession !== 'function' || typeof guard !== 'function') throw new TypeError('the session check needs a reader and a guard');
  if (typeof stop !== 'function' || typeof onUnpaired !== 'function') throw new TypeError('the session check needs stop and onUnpaired');
  let lastCheck = -Infinity;
  return async () => {
    if (now() - lastCheck < minGapMs) return;
    lastCheck = now();
    // Unreachable is expected while offline; only a definite 401 unpairs.
    const current = await readSession().catch(() => undefined);
    if (current === null) {
      stop();
      onUnpaired('This device is no longer paired.');
    } else if (current) {
      guard(current.device);
    }
  };
}
