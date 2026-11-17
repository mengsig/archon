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

// The stream and conversation recoveries: 2–4 s first, at most 30 s.
export const STREAM_BACKOFF = Object.freeze({ baseMs: 4000, capMs: 30_000 });

// The wait before automatic retry `attempt`: the backoff above, never sooner than
// `minMs` (a host's Retry-After) and spread past it by up to half again, capped.
export function retryDelay(attempt, { minMs = 0, random = Math.random, limits = BACKOFF } = {}) {
  if (!Number.isFinite(minMs) || minMs < 0) throw new RangeError(`minMs must be a non-negative wait, got ${minMs}`);
  const r = random();
  const backoff = backoffDelay(attempt, () => r, limits);
  const named = minMs + (r * Math.min(minMs, limits.capMs)) / 2;
  const delay = Math.round(Math.max(backoff, named));
  if (delay < minMs) throw new RangeError(`a ${delay} ms wait is shorter than the ${minMs} ms the host asked for`);
  return delay;
}
