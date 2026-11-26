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

test('a switch archond confirmed but the frames never reach is asked again once, then given up with a toast', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'phone' }));
  s = answer(s, 'phone', true);
  assert.ok(presetMismatch(s), 'the frames are still at Desktop');
  assert.equal(needsResend(s), true);
  assert.equal(send(reduce(s, { type: 'preset_resend' })).sent, null, 'the usual resend is spent: Phone was just sent');
  let sent;
  ({ s, sent } = send(reduce(s, { type: 'preset_stalled' })));
  assert.equal(sent, 'phone', 'the stalled switch is asked for once more');
  assert.equal(s.toasts.length, 0, 'no error while it is retried');
  s = answer(s, 'phone', true);
  ({ s, sent } = send(reduce(s, { type: 'preset_stalled' })));
  assert.equal(sent, 'desktop', 'then the size the frames show is asked for');
  assert.equal(s.preset, 'desktop');
  assert.equal(presetMismatch(s), false, 'nothing is left switching');
  assert.deepEqual(s.toasts, ['The preview did not switch to Phone, so it stays at Desktop. Pick a size to try again.']);
});

test('a stall check with a request in flight, or with the frames at the size, changes nothing', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'tablet' }));
  assert.equal(reduce(s, { type: 'preset_stalled' }), s, 'the request in flight answers first');
  s = reduce(answer(s, 'tablet', true), frame(834, 1112));
  assert.equal(reduce(s, { type: 'preset_stalled' }), s, 'the frames are at the confirmed size');
});

test('a new choice after a given-up switch gets its own retry', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'phone' }));
  s = answer(s, 'phone', true);
  ({ s } = send(reduce(s, { type: 'preset_stalled' })));
  s = answer(s, 'phone', true);
  ({ s } = send(reduce(s, { type: 'preset_stalled' })));
  s = reduce(answer(s, 'desktop', true), { type: 'toasts_taken' });
  ({ s } = send(reduce(s, { type: 'preset', preset: 'phone' })));
  s = answer(s, 'phone', true);
  const retried = send(reduce(s, { type: 'preset_stalled' }));
  assert.equal(retried.sent, 'phone', 'asked again, not given up at once');
  assert.equal(retried.s.toasts.length, 0);
});

test('a given-up switch whose fallback is refused still ends, and does not start again', () => {
  let { s } = send(reduce(live(), { type: 'preset', preset: 'phone' }));
  s = answer(s, 'phone', true);
  ({ s } = send(reduce(s, { type: 'preset_stalled' })));
  s = answer(s, 'phone', true);
  let sent;
  ({ s, sent } = send(reduce(s, { type: 'preset_stalled' })));
  assert.equal(sent, 'desktop');
  assert.equal(s.toasts.length, 1);
  // Refused: the app opened a dialog meanwhile. The frames still show Desktop.
  s = reduce(answer(s, 'desktop', false), { type: 'toasts_taken' });
  assert.equal(presetMismatch(s), false, 'the preview shows the size it is at, not "Switching…"');
  assert.equal(reduce(s, { type: 'preset_stalled' }), s, 'nothing is left to retry');
  // archond's view still says Phone: the frames' size is kept without asking or telling again.
  s = reduce(s, { type: 'view', view: view({ session: session({ viewport_preset: 'phone' }) }), stamp: s.viewport.settles });
  assert.ok(presetMismatch(s));
  ({ s, sent } = send(reduce(s, { type: 'preset_stalled' })));
  assert.equal(sent, null, 'no request loop');
  assert.equal(s.toasts.length, 0, 'no toast loop');
  assert.equal(presetMismatch(s), false);
});

test('a dialog answered and opened again with the same text is a new dialog', () => {
  const alert = { type: 'event', event: { event: 'dialog', kind: 'alert', message: 'Saved', default_prompt: null } };
  const first = reduce(live(), alert);
  const answered = reduce(first, { type: 'dialog_closed', dialog: first.dialog });
  assert.equal(answered.dialog, null);
  const second = reduce(answered, alert);
  assert.notEqual(second.dialog, first.dialog, 'the app opened it again');
  assert.equal(reduce(second, { type: 'dialog_closed', dialog: first.dialog }).dialog, second.dialog, 'the first one closing late leaves it');
});

test('a dialog archond shows again to a rejoining stream is one dialog', () => {
  const alert = { type: 'event', event: { event: 'dialog', kind: 'alert', message: 'Unsaved changes', default_prompt: null } };
  const once = reduce(live(), alert);
  const twice = reduce(once, alert);
  assert.equal(twice.dialog, once.dialog, 'the open dialog is kept, not replaced by its copy');
  const other = reduce(twice, { ...alert, event: { ...alert.event, message: 'Saved' } });
  assert.equal(other.dialog.message, 'Saved', 'a different dialog still replaces it');
});
