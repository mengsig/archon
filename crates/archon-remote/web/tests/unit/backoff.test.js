import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import { retryDelay, STREAM_BACKOFF, BACKOFF } from '../../src/core/backoff.js';

const low = () => 0;
const high = () => 0.9999;

test('a retry waits half its ceiling plus a random half, capped', () => {
  assert.equal(retryDelay(0, { random: low, limits: STREAM_BACKOFF }), 2000);
  assert.equal(retryDelay(0, { random: high, limits: STREAM_BACKOFF }), 4000);
  assert.equal(retryDelay(1, { random: () => 0.5, limits: STREAM_BACKOFF }), 6000);
  assert.equal(retryDelay(40, { random: low, limits: STREAM_BACKOFF }), 15_000);
  assert.ok(retryDelay(40, { random: high, limits: STREAM_BACKOFF }) <= 30_000, 'never past the cap');
  assert.notEqual(retryDelay(2, { random: low }), retryDelay(2, { random: high }), 'two tabs spread apart');
});

test('a host’s Retry-After is the least wait, spread by up to half again', () => {
  assert.equal(retryDelay(0, { minMs: 2000, random: low }), 2000);
  assert.equal(retryDelay(0, { minMs: 2000, random: high }), 3000);
  assert.equal(retryDelay(0, { minMs: 60_000, random: low }), 60_000);
  assert.equal(retryDelay(0, { minMs: 60_000, random: () => 0.5 }), 60_000 + BACKOFF.capMs / 4, 'the spread is capped');
  assert.equal(retryDelay(5, { minMs: 1000, random: low }), 8000, 'a later backoff outgrows a short Retry-After');
  for (const r of [0, 0.25, 0.5, 0.9999]) assert.ok(retryDelay(3, { minMs: 7000, random: () => r }) >= 7000);
});

test('a retry delay refuses bad input', () => {
  assert.throws(() => retryDelay(-1), RangeError);
  assert.throws(() => retryDelay(0, { minMs: -5 }), RangeError);
  assert.throws(() => retryDelay(0, { minMs: Number.NaN }), RangeError);
  assert.throws(() => retryDelay(0, { random: () => 1 }), RangeError);
});

// Runs boot-watch.js against a fake page with Math.random fixed; returns the
// countdown it shows and when it reloads.
function bootWatch({ random, attempts = 0 }) {
  const source = readFileSync(new URL('../../src/boot-watch.js', import.meta.url), 'utf8');
  const timers = [];
  const storage = new Map([['archon-boot-attempts', String(attempts)]]);
  const node = () => ({ setAttribute() {}, append() {}, textContent: '' });
  const status = node();
  let reloads = 0;
  let clock = 0;
  const page = {
    Math: Object.assign(Object.create(Math), { random }),
    Date: { now: () => clock },
    setTimeout: (fn, ms) => timers.push({ fn, at: clock + ms }),
    sessionStorage: { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v), removeItem: (k) => storage.delete(k) },
    location: { reload: () => { reloads += 1; } },
    window: { addEventListener() {} },
    HTMLScriptElement: class {},
    document: {
      documentElement: { dataset: {} },
      getElementById: () => ({ replaceChildren() {} }),
      createElement: (tag) => (tag === 'p' ? status : node()),
    },
  };
  vm.runInNewContext(source, page);
  const advance = (ms) => {
    const until = clock + ms;
    for (let next; (next = timers.sort((a, b) => a.at - b.at)[0]) && next.at <= until;) {
      timers.shift();
      clock = next.at;
      next.fn();
    }
    clock = until;
  };
  advance(8000);
  return { status, advance, reloads: () => reloads, attempts: () => storage.get('archon-boot-attempts') };
}

test('the boot watch reloads at a jittered moment within its ceiling', () => {
  const early = bootWatch({ random: low });
  assert.equal(early.attempts(), '1');
  assert.match(early.status.textContent, /Retrying in 2 s/);
  early.advance(1999);
  assert.equal(early.reloads(), 0);
  early.advance(1);
  assert.equal(early.reloads(), 1, 'the lowest draw waits half the 4 s ceiling');

  const late = bootWatch({ random: () => 0.75 });
  assert.match(late.status.textContent, /Retrying in 4 s/);
  late.advance(3499);
  assert.equal(late.reloads(), 0, 'a higher draw waits longer: 3.5 s');
  late.advance(1);
  assert.equal(late.reloads(), 1);

  const capped = bootWatch({ random: high, attempts: 6 });
  capped.advance(29_990);
  assert.equal(capped.reloads(), 0);
  capped.advance(10);
  assert.equal(capped.reloads(), 1, 'never past the 30 s cap');
});
