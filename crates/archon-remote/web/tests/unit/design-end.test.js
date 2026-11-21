import test from 'node:test';
import assert from 'node:assert/strict';
import {
  initialState, reduce, stagePhase, dotLook, endInfo, followsNewSession, noteActions, sessionLabel, earlierText,
  confirmedPreset, presetMismatch, pointerBlockedText, notesHeading, PRESET_SIZE,
} from '../../src/features/design/state.js';
import { designErrorText } from '../../src/core/design.js';

const session = (over = {}) => ({ id: 'ds_1', state: 'live', degraded: false, note_version: 3, viewport_preset: 'desktop', ...over });
const view = (over = {}) => ({ session: session(), notes: [], browser: { status: 'running' }, ...over });
const loaded = (v = view()) => reduce(initialState('qi_1'), { type: 'view', view: v });
const ended = (end_reason) => loaded(view({ session: session({ state: 'ended', end_reason }) }));

test('F4: each end reason has its own title, next step and dot label', () => {
  const want = {
    approved: ['Design approved', /building it now/, 'Approved'],
    revised: ['Changes requested', /new design session starts/, 'Changes requested'],
    rejected: ['Design rejected', /stops here/, 'Rejected'],
    cancelled: ['Session cancelled', /cancelled/, 'Cancelled'],
    failed: ['Session failed', /failed/, 'Failed'],
    'timed out': ['Session timed out', /time/, 'Timed out'],
    completed: ['Session completed', /complete/, 'Completed'],
  };
  for (const [reason, [title, text, dot]] of Object.entries(want)) {
    const s = ended(reason);
    const p = stagePhase(s);
    assert.equal(p.kind, 'ended');
    assert.equal(p.title, title);
    assert.match(p.text, text);
    assert.equal(dotLook(s).label, dot);
  }
  assert.equal(dotLook(ended(undefined)).label, 'Ended');
  assert.equal(endInfo('anything else').title, 'This design session ended');
});

test('F4: an ended event maps its reason; a bare ended does not wipe a known one', () => {
  let s = reduce(loaded(), { type: 'event', event: { event: 'ended', reason: 'rejected' } });
  assert.equal(stagePhase(s).title, 'Design rejected');
  s = reduce(s, { type: 'event', event: { event: 'ended', reason: 'ended' } });
  assert.equal(stagePhase(s).title, 'Design rejected');
  const bare = reduce(loaded(), { type: 'event', event: { event: 'ended', reason: 'ended' } });
  assert.equal(bare.refresh, true, 'asks the daemon for the real reason');
});

test('F2: the screen follows the gate and a revision until a new session opens', () => {
  assert.equal(followsNewSession(loaded(view({ session: session({ state: 'confirmed' }) }))), true);
  assert.equal(followsNewSession(ended('revised')), true);
  assert.equal(followsNewSession(ended('approved')), false);
  assert.equal(followsNewSession(ended('cancelled')), false);
  assert.equal(followsNewSession(loaded()), false);
  const revised = ended('revised');
  const next = reduce(revised, { type: 'view', view: view({ session: session({ id: 'ds_2', state: 'starting' }) }) });
  assert.equal(followsNewSession(next), false, 'polling stops once the new session is loaded');
});

test('F2: a new session id resets the picture and selection and toasts once', () => {
  let s = reduce(ended('revised'), { type: 'event', event: { event: 'frame', seq: 5, w: 390, h: 844, at: 'x' } });
  s = { ...s, selection: { box: { x: 1, y: 1, w: 1, h: 1 }, label: 'a' }, progress: 'old' };
  s = reduce(s, { type: 'view', view: view({ session: session({ id: 'ds_2', state: 'starting' }) }) });
  assert.equal(s.frame, null);
  assert.equal(s.selection, null);
  assert.equal(s.progress, '');
  assert.equal(s.ended, null);
  assert.deepEqual(s.toasts, ['A new design session started']);
  const again = reduce(s, { type: 'view', view: view({ session: session({ id: 'ds_2' }) }) });
  assert.equal(again.toasts.length, 1, 'the same session does not toast again');
});

test('F2: the confirmed state gives way to the end state when the session ends', () => {
  const s = loaded(view({ session: session({ state: 'confirmed' }) }));
  assert.equal(stagePhase(s).kind, 'confirmed');
  const done = reduce(s, { type: 'event', event: { event: 'session', state: 'ended', end_reason: 'approved', degraded: false, note_version: 3 } });
  assert.equal(stagePhase(done).title, 'Design approved');
});

test('F7: an ended session never falls back to "no live design session"', () => {
  let s = reduce(loaded(), { type: 'event', event: { event: 'ended', reason: 'cancelled' } });
  s = reduce(s, { type: 'view', view: { session: null, notes: [], browser: null } });
  const p = stagePhase(s);
  assert.equal(p.kind, 'ended');
  assert.equal(p.title, 'Session cancelled');
  assert.doesNotMatch(p.text, /no live design session/);
});

test('F7: the conflict copy is accurate', () => {
  assert.doesNotMatch(designErrorText({ code: 'conflict' }), /Another device/);
  assert.match(designErrorText({ code: 'conflict' }), /already moved on/);
});

test('F3: earlier-session notes are labelled and may be undone by the daemon rule', () => {
  const old = (over = {}) => ({ id: 'D1', status: 'done', commit: 'abc', read_only: true, approved: false, ...over });
  assert.equal(sessionLabel(old()), 'Earlier session');
  assert.match(earlierText(old()), /can’t be edited/);
  assert.equal(noteActions(old(), { live: true }).undo, true);
  assert.equal(noteActions(old(), { live: true }).edit, false);
  assert.equal(noteActions(old(), { live: false }).undo, false, 'no undo while not live');
  assert.equal(noteActions(old(), { live: true, finishing: true }).undo, false);
  assert.equal(noteActions(old({ approved: true }), { live: true }).undo, false);
  assert.equal(noteActions(old({ undone_by: 'D3' }), { live: true }).undo, false);
  assert.equal(noteActions(old({ no_change: true }), { live: true }).undo, false);
  assert.equal(noteActions(old({ commit: null }), { live: true }).undo, false);
});

test('F5: a preset is confirmed only by the frame size', () => {
  assert.deepEqual(PRESET_SIZE.desktop, { w: 1440, h: 900 });
  assert.deepEqual(PRESET_SIZE.tablet, { w: 834, h: 1112 });
  assert.equal(confirmedPreset({ w: 390, h: 844 }), 'phone');
  assert.equal(confirmedPreset({ w: 1440, h: 1000 }), null);
  // A slow link streams frames scaled down; their shape still proves the preset.
  assert.equal(confirmedPreset({ w: 720, h: 450 }), 'desktop');
  assert.equal(confirmedPreset({ w: 195, h: 422 }), 'phone');
  assert.equal(confirmedPreset({ w: 1440, h: 900 }), 'desktop');
  assert.equal(confirmedPreset(null), null);
  const phone = { ...loaded(view({ session: session({ viewport_preset: 'phone' }) })), frame: { seq: 1, w: 1440, h: 900 } };
  assert.equal(presetMismatch(phone), true);
  assert.equal(pointerBlockedText(phone), 'Switching to Phone…');
  const ok = { ...phone, frame: { seq: 2, w: 390, h: 844 } };
  assert.equal(presetMismatch(ok), false);
  assert.equal(pointerBlockedText(ok), '');
  assert.equal(presetMismatch({ ...phone, frame: null }), false, 'nothing to compare before the first frame');
});

test('F8: the notes heading counts the notes listed', () => {
  const notes = [1, 2, 3, 4, 5, 6].map((n) => ({ id: `D${n}`, kind: n === 4 ? 'revert' : 'change' }));
  assert.equal(notesHeading(notes), 'Notes (6)');
});
