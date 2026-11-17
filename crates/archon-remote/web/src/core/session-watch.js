// Asks the session endpoint whether this tab still has the device and role it was built for.
// States: sure (controls live), unsure (controls withheld), stopped (terminal).
// check():   at most one read per `minGapMs`; its failure is no answer and is not retried.
// recheck(): the page is shown: read now unless a retrying read is in flight. Each failed
//            read counts and is retried after a backoff; `unsureAfter` of them: unsure.
// signal():  another tab paired or forgot: unsure at once, a new round, then as recheck().
// answer:    any read of the current round ends it (older reads and the retry are void) and
//            zeroes the count. Same device: sure. Changed: remount. 401: unpaired. Both stop.
// cancel():  stopped: no later read, retry or callback.
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

export function sessionChecker(options) {
  return new SessionChecker(options);
}

class SessionChecker {
  constructor({ readSession, guard, stop, onUnpaired, onUnsure = () => {}, minGapMs, now = Date.now, unsureAfter = 4, limits = BACKOFF }) {
    assertCheckerArgs({ readSession, guard, stop, onUnpaired, onUnsure, minGapMs, unsureAfter });
    Object.assign(this, { readSession, guard, stop, onUnpaired, onUnsure, minGapMs, now, unsureAfter, limits });
    this.alive = true;
    this.unsure = false;
    this.lastCheck = -Infinity;
    // A read carries the round it began in; one from an older round is ignored.
    this.round = 0;
    // Failed retrying reads since the last answer.
    this.failures = 0;
    // The round's one retrying read is in flight (`retrying`) or waits out a backoff (`timer`).
    this.retrying = false;
    this.timer = null;
  }

  async check() {
    if (!this.alive || this.now() - this.lastCheck < this.minGapMs) return;
    this.lastCheck = this.now();
    await this.read(false);
  }

  async recheck() {
    if (!this.alive || this.retrying) return;
    clearTimeout(this.timer);
    this.timer = null;
    await this.retryRead();
  }

  async signal() {
    if (!this.alive) return;
    this.markUnsure(true);
    // A read begun before the signal may carry the session it replaced.
    this.endRound();
    await this.retryRead();
  }

  cancel() {
    this.alive = false;
    this.endRound();
  }

  markUnsure(value) {
    if (this.unsure === value) return;
    this.unsure = value;
    this.onUnsure(value);
  }

  endRound() {
    clearTimeout(this.timer);
    this.timer = null;
    this.retrying = false;
    this.round += 1;
  }

  retryRead() {
    if (this.retrying || this.timer !== null) throw new Error('a session retry is already under way');
    this.retrying = true;
    this.lastCheck = this.now();
    return this.read(true);
  }

  async read(retry) {
    const round = this.round;
    // Unreachable, busy or timed out is no answer: only a session or a 401 (null) is.
    const outcome = await this.readSession().then((session) => ({ session }), (error) => ({ error }));
    if (!this.alive || round !== this.round) return;
    if (!('error' in outcome)) {
      this.answered(outcome.session);
      return;
    }
    if (!retry) return;
    this.retrying = false;
    this.failed(outcome.error);
  }

  answered(session) {
    if (session !== null && !session?.device) throw new TypeError('a session answer is null or names its device');
    this.endRound();
    this.failures = 0;
    if (session === null) {
      this.cancel();
      this.stop();
      this.onUnpaired('This device is no longer paired.');
      return;
    }
    // A changed device remounts the app; controls already withheld stay so until it has.
    if (this.guard(session.device)) {
      this.cancel();
      return;
    }
    this.markUnsure(false);
  }

  failed(error) {
    if (this.retrying || this.timer !== null) throw new Error('a session retry is already under way');
    this.failures += 1;
    if (this.failures >= this.unsureAfter) this.markUnsure(true);
    // A busy host's own wait is the least, as at boot.
    const namedMs = Number.isFinite(error?.retryAfterSecs) && error.retryAfterSecs > 0 ? error.retryAfterSecs * 1000 : 0;
    const delay = retryDelay(this.failures - 1, { minMs: namedMs, limits: this.limits });
    this.timer = setTimeout(() => {
      this.timer = null;
      this.retryRead();
    }, delay);
  }
}

function assertCheckerArgs({ readSession, guard, stop, onUnpaired, onUnsure, minGapMs, unsureAfter }) {
  if (typeof readSession !== 'function' || typeof guard !== 'function') throw new TypeError('the session check needs a reader and a guard');
  if (typeof stop !== 'function' || typeof onUnpaired !== 'function') throw new TypeError('the session check needs stop and onUnpaired');
  if (typeof onUnsure !== 'function') throw new TypeError('onUnsure reports a session the app cannot confirm');
  if (!Number.isFinite(minGapMs) || minGapMs < 0) throw new RangeError(`minGapMs must be a wait, got ${minGapMs}`);
  if (!Number.isInteger(unsureAfter) || unsureAfter < 1) throw new RangeError(`unsureAfter must be a positive count, got ${unsureAfter}`);
}
