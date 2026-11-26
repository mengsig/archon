import test from 'node:test';
import assert from 'node:assert/strict';
import { initialState, reduce, presetMismatch, needsResend } from '../../src/features/design/state.js';
import { nextRequest } from '../../src/features/design/viewport.js';

const session = (over = {}) => ({ id: 'ds_1', state: 'live', degraded: false, note_version: 3, viewport_preset: 'desktop', ...over });
const view = (over = {}) => ({ session: session(), notes: [], browser: { status: 'running' }, finish_blocker: null, images_used: 0, images_max: 128, stage_deadline: null, ...over });
const frame = (w, h) => ({ type: 'event', event: { event: 'frame', seq: 1, w, h, at: 'x' } });
const live = () => reduce(reduce(initialState('qi_1'), { type: 'view', view: view(), stamp: 0 }), frame(1440, 900));

// What the screen does after each render: send the one request due, if any.
function send(s, id = 'ds_1') {
  const preset = nextRequest(s.viewport);
  return preset ? { s: reduce(s, { type: 'preset_sending', session: id, preset }), sent: preset } : { s, sent: null };
}
const answer = (s, preset, ok, id = 'ds_1') => reduce(s, { type: 'preset_settled', session: id, preset, ok });

test('a preset chosen over a frame is one request, and the chosen preset shows meanwhile', () => {
  let { s, sent } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  assert.equal(sent, 'tablet');
  assert.equal(s.preset, 'tablet');
  assert.ok(presetMismatch(s), 'switching until a tablet frame comes');
  assert.equal(send(s).sent, null, 'nothing more while it is in flight');
  s = answer(s, 'tablet', true);
  assert.deepEqual([s.viewport.confirmed, s.viewport.pending, nextRequest(s.viewport)], ['tablet', null, null]);
});

test('a refusal after a success goes back to the success, and switching ends', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  s = reduce(answer(s, 'tablet', true), frame(834, 1112));
  ({ s } = send(reduce(s, { type: 'preset', preset: 'phone' })));
  s = answer(s, 'phone', false);
  assert.equal(s.preset, 'tablet');
  assert.equal(presetMismatch(s), false, 'the tablet frame matches again');
});

test('overlapping choices: one request at a time, and only the newest is sent next', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  s = reduce(reduce(s, { type: 'preset', preset: 'desktop' }), { type: 'preset', preset: 'phone' });
  assert.equal(s.preset, 'phone');
  assert.equal(send(s).sent, null);
  let sent;
  ({ s, sent } = send(answer(s, 'tablet', true)));
  assert.equal(sent, 'phone', 'desktop was replaced before it was sent');
  s = answer(s, 'phone', true);
  assert.equal(s.viewport.confirmed, 'phone');
});

test('overlapping choices both refused go back to the confirmed preset', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  s = reduce(s, { type: 'preset', preset: 'phone' });
  let sent;
  ({ s, sent } = send(answer(s, 'tablet', false)));
  assert.deepEqual([sent, s.preset], ['phone', 'phone'], 'the newer choice still goes out');
  s = answer(s, 'phone', false);
  assert.deepEqual([s.preset, s.viewport.confirmed, presetMismatch(s), nextRequest(s.viewport)], ['desktop', 'desktop', false, null]);
});

test('a choice equal to what archond will have once answered is dropped', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  s = reduce(s, { type: 'preset', preset: 'desktop' });
  s = answer(s, 'tablet', false);
  assert.deepEqual([s.preset, nextRequest(s.viewport)], ['desktop', null], 'refused tablet: desktop stands, nothing to send');
});

test('a view asked for before a resize was answered does not undo it', () => {
  let s = live();
  const stamp = s.viewport.settles;
  ({ s } = send(reduce(s, { type: 'preset', preset: 'tablet' })));
  s = reduce(s, { type: 'view', view: view(), stamp });
  assert.equal(s.preset, 'tablet', 'ignored while the change is in flight');
  s = answer(s, 'tablet', true);
  s = reduce(s, { type: 'view', view: view(), stamp });
  assert.equal(s.viewport.confirmed, 'tablet', 'ignored once answered too');
  s = reduce(s, { type: 'view', view: view({ session: session({ viewport_preset: 'phone' }) }), stamp: s.viewport.settles });
  assert.equal(s.viewport.confirmed, 'phone', 'a view asked for afterwards is current');
});

test('a dialog-open refusal leaves nothing switching and the next choice is sent', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  assert.ok(presetMismatch(s));
  s = answer(s, 'tablet', false);
  assert.deepEqual([s.preset, presetMismatch(s)], ['desktop', false]);
  assert.equal(send(reduce(s, { type: 'preset', preset: 'tablet' })).sent, 'tablet');
});

test('a frame at another size re-sends the confirmed preset once, and never loops', () => {
  let s = reduce(live(), frame(390, 844));
  assert.ok(needsResend(s));
  s = reduce(s, { type: 'preset_resend' });
  let sent;
  ({ s, sent } = send(s));
  assert.equal(sent, 'desktop');
  s = answer(s, 'desktop', false);
  assert.equal(reduce(s, { type: 'preset_resend' }).viewport.queued, null, 'refused once: not again');
});

test("an answer for an earlier session's request changes nothing in the new one", () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  s = reduce(s, { type: 'view', view: view({ session: session({ id: 'ds_2', viewport_preset: 'phone' }) }), stamp: 0 });
  assert.deepEqual([s.preset, s.viewport.pending], ['phone', null]);
  s = answer(s, 'tablet', true);
  assert.equal(s.viewport.confirmed, 'phone');
  assert.throws(() => reduce(s, { type: 'preset', preset: 'watch' }), RangeError);
});
