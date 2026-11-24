import test from 'node:test';
import assert from 'node:assert/strict';
import {
  initialState, reduce, stagePhase, dotLook, endInfo, followsNewSession, noteActions, sessionLabel, earlierText,
  confirmedPreset, presetMismatch, pointerBlockedText, notesHeading, PRESET_SIZE, ledgerLede,
} from '../../src/features/design/state.js';
import { designErrorText } from '../../src/core/design.js';
import { gateChoices, gateQuestionFor } from '../../src/features/design/finish.js';

test('approval uses only the open proposal gate for the exact design attempt', () => {
  const current = { run: 'r1', stage_id: 'design', attempt: 2 };
  const gate = { item: 'i', ...current, subject: 'proposal', question: 'q1', decision: null, withdrawn: null };
  const questions = [{ id: 'other', item: 'i', answer: null }, { id: 'q1', item: 'i', answer: null }];
  assert.equal(gateQuestionFor(questions, [gate], current, 'i').id, 'q1');
  for (const change of [{ run: 'r2' }, { stage_id: 'other' }, { attempt: 1 }, { subject: 'report' }, { withdrawn: 'ended' }, { decision: {} }]) {
    assert.equal(gateQuestionFor(questions, [{ ...gate, ...change }], current, 'i'), null);
  }
  assert.equal(gateQuestionFor(questions, [gate, gate], current, 'i'), null);
  assert.equal(gateQuestionFor(questions, [gate], {}, 'i'), null);
});

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

test('GATE: a session shown as it ended keeps its decision, and its own notes are not called earlier', () => {
  // The daemon answers design_get with the item's last session once none is open.
  const approved = ended('approved');
  assert.equal(stagePhase(approved).title, 'Design approved');
  assert.equal(dotLook(approved).label, 'Approved');
  assert.equal(stagePhase(ended('rejected')).title, 'Design rejected');
  const own = { id: 'D1', session: 'ds_1', status: 'done', commit: 'abc', read_only: true };
  assert.equal(sessionLabel(own, 'ds_1'), '', 'its own note');
  assert.match(earlierText(own, 'ds_1'), /has ended/);
  assert.equal(sessionLabel({ ...own, session: 'ds_0' }, 'ds_1'), 'Earlier session');
  assert.equal(noteActions(own, { live: false }).undo, false, 'nothing to do on an ended session');
});

test('GATE: while the approval question is on its way the ledger never says none is open', () => {
  assert.doesNotMatch(ledgerLede(null), /No question/);
  assert.match(ledgerLede(null), /wrapping up/);
  assert.match(ledgerLede({ id: 'q1' }), /Approve it/);
  // An observer never gets the buttons, so it is not promised them.
  assert.doesNotMatch(ledgerLede(null, false), /appear here/);
  assert.match(ledgerLede(null, false), /operator device/);
});

test('item 7: a daemon refusal reads as a sentence, without its error kind', () => {
  assert.equal(designErrorText({ code: 'invalid', message: 'invalid argument: nothing to approve; add a note or cancel' }), 'Nothing to approve; add a note or cancel.');
  assert.equal(designErrorText({ code: 'forbidden', message: 'forbidden: only the operator can finish' }), 'Only the operator can finish.');
  assert.equal(designErrorText({ code: 'x', message: 'Already a sentence.' }), 'Already a sentence.');
  assert.equal(designErrorText({ code: 'invalid', message: 'invalid argument: ' }), 'Something went wrong.');
  assert.equal(designErrorText({ code: 'x', message: 'store: disk full: no space left' }), 'Disk full: no space left.');
  assert.equal(designErrorText({ code: 'x', message: 'io: read picture a.png: denied' }), 'Read picture a.png: denied.');
  assert.equal(designErrorText({ code: 'x', message: 'aether: the agent is gone' }), 'The agent is gone.');
  assert.equal(designErrorText({}), 'Something went wrong.');
});

test('item 8: an approved design says when the item is built, stopped or cancelled', () => {
  const approved = (itemState) => {
    const s = reduce(ended('approved'), { type: 'item_state', state: itemState });
    return { phase: stagePhase(s), dot: dotLook(s) };
  };
  assert.match(approved('running').phase.text, /building it now/);
  const built = approved('succeeded');
  assert.equal(built.phase.title, 'Design approved and built');
  assert.doesNotMatch(built.phase.text, /building it now/);
  assert.equal(built.dot.label, 'Built');
  assert.match(approved('failed').phase.text, /stopped after the design was approved/);
  assert.match(approved('cancelled').phase.text, /cancelled after the design was approved/);
  assert.equal(reduce(ended('approved'), { type: 'item_state', state: null }).itemState, null);
});

test('each proposal option sends its declared value with revision text only when asked', () => {
  const [approve, revise, reject] = gateChoices({ options: ['reject', 'revise', 'approve'] });
  assert.deepEqual([approve.id, revise.id, reject.id], ['approve', 'revise', 'reject']);
  assert.equal(approve.send('ignored'), 'approve');
  assert.equal(revise.send('More contrast'), 'revise: More contrast');
  assert.equal(reject.send('ignored'), 'reject');
  assert.deepEqual([approve, revise, reject].map((choice) => Boolean(choice.needsText)), [false, true, false]);
});
