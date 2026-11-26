import test from 'node:test';
import assert from 'node:assert/strict';
import { BEHIND, NOTICE_MS, createBehindLine } from '../../src/features/design/behind.js';

function line() {
  const shown = [];
  const timers = [];
  const behind = createBehindLine({
    show: (text) => shown.push(text),
    setTimer: (fn, ms) => { const t = { fn, ms }; timers.push(t); return t; },
    clearTimer: (t) => { const i = timers.indexOf(t); if (i >= 0) timers.splice(i, 1); },
  });
  return { behind, shown, timers, fire: () => timers.shift().fn() };
}

test('waiting shows one busy line until it stops, saying each change once', () => {
  const l = line();
  l.behind.waiting(true);
  l.behind.waiting(true);
  l.behind.waiting(false);
  assert.deepEqual(l.shown, [BEHIND.busy, null]);
  assert.equal(l.timers.length, 0, 'waiting never expires on its own');
});

test('catching up never wipes a notice that input was lost; it expires on its own', () => {
  const l = line();
  l.behind.waiting(true);
  l.behind.notice(BEHIND.lost);
  l.behind.waiting(false);
  assert.deepEqual(l.shown, [BEHIND.busy, BEHIND.lost], 'the notice stays while waiting ends');
  assert.equal(l.timers[0].ms, NOTICE_MS);
  l.fire();
  assert.deepEqual(l.shown, [BEHIND.busy, BEHIND.lost, null]);
});

test('a notice gives way to waiting that is still going on, and a newer notice replaces it', () => {
  const l = line();
  l.behind.notice(BEHIND.skipped);
  l.behind.notice(BEHIND.lost);
  assert.equal(l.timers.length, 1, 'one line, one timer: notices never stack');
  l.behind.waiting(true);
  l.fire();
  assert.deepEqual(l.shown, [BEHIND.skipped, BEHIND.lost, BEHIND.busy]);
  l.behind.stop();
  assert.equal(l.timers.length, 0);
  assert.throws(() => l.behind.notice(''), TypeError);
});
