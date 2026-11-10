// Capped exponential backoff with "equal jitter": half fixed, half random,
// so reconnecting clients spread out yet never retry instantly.
export const BACKOFF = Object.freeze({ baseMs: 500, capMs: 30_000 });
export const DISABLED_RETRY_MS = 60_000;

export function backoffDelay(attempt, random = Math.random, { baseMs, capMs } = BACKOFF) {
  if (!Number.isInteger(attempt) || attempt < 0) throw new RangeError(`attempt must be a non-negative integer, got ${attempt}`);
  const r = random();
  if (!(r >= 0 && r < 1)) throw new RangeError(`random() must be in [0, 1), got ${r}`);
  const ceiling = Math.min(capMs, baseMs * 2 ** Math.min(attempt, 30));
  return Math.round(ceiling / 2 + (r * ceiling) / 2);
}
