import test from 'node:test';
import assert from 'node:assert/strict';
import { initialState, reduce, stagePhase, noteLook, noteActions, statusLine, imageCapWarning, failText, captionFor, currentPath } from '../../src/features/design/state.js';

test('an initial load failure is visible instead of pretending there is no session', () => {
  const failed = reduce(initialState('i'), { type: 'error', error: 'Connection failed.' });
  assert.equal(stagePhase(failed).kind, 'error');
  assert.equal(stagePhase(failed).text, 'Connection failed.');
});

const session = (over = {}) => ({ id: 'ds_1', state: 'live', degraded: false, note_version: 3, viewport_preset: 'desktop', ...over });
const note = (n, over = {}) => ({ id: `D${n}`, number: n, status: 'queued', kind: 'change', read_only: false, updated_at: new Date().toISOString(), ...over });
const view = (over = {}) => ({ session: session(), notes: [note(1), note(2)], browser: { status: 'running' }, finish_blocker: null, images_used: 1, images_max: 128, stage_deadline: null, ...over });
const loaded = (v = view()) => reduce(initialState('qi_1'), { type: 'view', view: v });

test('a view loads notes newest first and the session facts', () => {
  const s = loaded();
  assert.deepEqual(s.notes.map((n) => n.id), ['D2', 'D1']);
  assert.equal(s.noteVersion, 3);
  assert.equal(s.imagesMax, 128);
  assert.equal(stagePhase(s).kind, 'starting', 'no frame yet');
  assert.equal(stagePhase(reduce(s, { type: 'event', event: { event: 'frame', seq: 1, w: 10, h: 10, at: 'x' } })).kind, 'live');
});

test('note events upsert by id and keep the order', () => {
  let s = loaded();
  s = reduce(s, { type: 'event', event: { event: 'note', ...note(2, { status: 'working' }) } });
  s = reduce(s, { type: 'event', event: { event: 'note', ...note(3) } });
  assert.deepEqual(s.notes.map((n) => [n.id, n.status]), [['D3', 'queued'], ['D2', 'working'], ['D1', 'queued']]);
});

test('a session event with a newer note version asks for a reload', () => {
  const s = reduce(loaded(), { type: 'event', event: { event: 'session', state: 'live', degraded: false, preview_path: null, commit: null, browser: { status: 'running' }, note_version: 9 } });
  assert.equal(s.refresh, true);
  assert.equal(s.noteVersion, 9);
  assert.equal(reduce(s, { type: 'view', view: view() }).refresh, false);
});

test('toasts, dialogs, progress and blocked hosts are kept for the screen', () => {
  let s = loaded();
  s = reduce(s, { type: 'event', event: { event: 'toast', message: 'hi' } });
  s = reduce(s, { type: 'event', event: { event: 'dialog', kind: 'confirm', message: 'Sure?', default_prompt: null } });
  s = reduce(s, { type: 'event', event: { event: 'agent_progress', line: 'npm run dev' } });
  s = reduce(s, { type: 'event', event: { event: 'blocked_hosts', hosts: ['x.test'] } });
  assert.deepEqual([s.toasts, s.dialog.message, s.progress, s.blocked], [['hi'], 'Sure?', 'npm run dev', ['x.test']]);
  assert.deepEqual(reduce(s, { type: 'toasts_taken' }).toasts, []);
  assert.equal(reduce(s, { type: 'dialog_closed', dialog: s.dialog }).dialog, null);
});

test('only the latest refused preset rolls back, to the one archond last applied', () => {
  let s = reduce(loaded(), { type: 'preset', preset: 'tablet', ask: 1 });
  s = reduce(s, { type: 'preset_applied', preset: 'tablet', ask: 1 });
  s = reduce(reduce(s, { type: 'preset', preset: 'phone', ask: 2 }), { type: 'preset', preset: 'desktop', ask: 3 });
  s = reduce(s, { type: 'preset_refused', ask: 2 });
  assert.equal(s.preset, 'desktop', 'an older refusal leaves the newer choice');
  s = reduce(s, { type: 'preset_refused', ask: 3 });
  assert.equal(s.preset, 'tablet', 'the applied tablet, not the stale session desktop');
});

test('a late success of an older preset does not overwrite a newer one', () => {
  let s = reduce(loaded(), { type: 'preset_applied', preset: 'phone', ask: 2 });
  s = reduce(s, { type: 'preset_applied', preset: 'tablet', ask: 1 });
  s = reduce(s, { type: 'preset', preset: 'desktop', ask: 3 });
  assert.equal(reduce(s, { type: 'preset_refused', ask: 3 }).preset, 'phone');
});

test('a dialog the app opens while the last one closes is kept', () => {
  const dialog = (message) => ({ type: 'event', event: { event: 'dialog', kind: 'alert', message, default_prompt: null } });
  const first = reduce(loaded(), dialog('first'));
  const second = reduce(first, dialog('second'));
  const closed = reduce(second, { type: 'dialog_closed', dialog: first.dialog });
  assert.equal(closed.dialog?.message, 'second');
  assert.equal(reduce(closed, { type: 'dialog_closed', dialog: closed.dialog }).dialog, null);
});

test('every sec 7.3 state shows its text', () => {
  const at = (v, extra = {}) => ({ ...reduce(initialState('i'), { type: 'view', view: v }), frame: { seq: 1 }, ...extra });
  assert.equal(stagePhase(at(view({ session: session({ state: 'starting' }) }), { frame: null })).title, 'Starting your app…');
  assert.equal(stagePhase(at(view())).kind, 'live');
  const missing = stagePhase(at(view({ browser: { status: 'missing', hint: 'Install chromium.' } })));
  assert.match(missing.text, /Chrome or Chromium on this machine/);
  assert.equal(missing.detail, 'Install chromium.', 'the doctor detail is kept apart from the sentence');
  assert.match(stagePhase(at(view({ browser: { status: 'sandbox_unavailable', hint: '' } }))).text, /sandbox is unavailable/);
  assert.equal(stagePhase(at(view({ browser: { status: 'queued_for_slot' } }))).title, 'Queued for a preview slot');
  assert.equal(stagePhase(at(view(), { connection: 'reconnecting' })).title, 'Reconnecting…');
  assert.equal(stagePhase(at(view({ session: session({ degraded: true }) }), { degraded: true })).kind, 'preview_down');
  assert.equal(stagePhase(at(view({ session: session({ state: 'finishing' }) }))).kind, 'finishing');
  assert.equal(stagePhase(at(view({ session: session({ state: 'ended', end_reason: 'approved' }) }))).title, 'Design approved');
  assert.match(stagePhase({ ...initialState('i'), loaded: true, observer: { state: 'live', noteCount: 1 } }).text, /Only the operator’s devices/);
  assert.equal(stagePhase(initialState('i'), { features: false }).kind, 'unavailable');
});

test('status chips carry an icon and a word', () => {
  const words = Object.entries({ queued: 'Queued', sent: 'Working', working: 'Working', needs_reply: 'Needs reply', done: 'Done', failed: 'Failed', cancelled: 'Cancelled' });
  for (const [status, word] of words) {
    const look = noteLook(note(1, { status }));
    assert.equal(look.label, word);
    assert.ok(look.icon);
  }
  assert.equal(noteLook(note(1, { status: 'done', undone_by: 'D4' })).label, 'Undone');
  assert.equal(noteLook(note(1, { status: 'failed', fail_reason: 'not_delivered' })).label, 'Not delivered');
});

test('actions follow the note: edit while queued, drop after 15 minutes, undo when done', () => {
  assert.deepEqual(Object.entries(noteActions(note(1))).filter(([, v]) => v).map(([k]) => k), ['edit', 'cancel']);
  assert.equal(noteActions(note(1, { status: 'needs_reply' })).drop, true);
  const old = new Date(Date.now() - 16 * 60000).toISOString();
  assert.equal(noteActions(note(1, { status: 'working', updated_at: old })).drop, true);
  assert.equal(noteActions(note(1, { status: 'working' })).drop, false);
  assert.equal(noteActions(note(1, { status: 'done', commit: 'abc' })).undo, true);
  assert.equal(noteActions(note(1, { status: 'done', commit: 'abc', no_change: true })).undo, false);
  assert.equal(noteActions(note(1, { status: 'done', commit: 'abc', undone_by: 'D5' })).undo, false);
  assert.equal(noteActions(note(1, { status: 'failed', fail_reason: 'not_delivered' })).resend, true);
  assert.equal(noteActions(note(1, { status: 'done', commit: 'abc', read_only: true })).undo, false);
  assert.equal(noteActions(note(1, { status: 'queued' }), { finishing: true }).edit, false);
});

test('the status line names the busy note and the one waiting for an answer', () => {
  const s = (notes) => ({ ...loaded(), notes, progress: '' });
  assert.match(statusLine(s([note(3, { status: 'working' })])), /Agent is working on D3\. New notes wait in line\./);
  assert.match(statusLine(s([note(3, { status: 'needs_reply' })])), /Waiting for your answer on D3/);
});

test('failure text is plain; the image cap warns before it blocks', () => {
  assert.match(failText(note(1, { status: 'failed', fail_reason: 'undo_conflict' })), /Can’t undo automatically/);
  assert.equal(imageCapWarning(10, 128, 5), null);
  assert.match(imageCapWarning(120, 128, 4), /only 4 more fit/);
  assert.match(imageCapWarning(126, 128, 5), /Switch some pictures off/);
  assert.match(captionFor('3f9c2a1deadbeef', '2026-01-01T14:32:00Z'), /^captured from the preview at commit 3f9c2a1, /);
});

test('confirmed is waiting for approval, never ended or approved', () => {
  const s = reduce(initialState('i'), { type: 'view', view: { session: session({ state: 'confirmed' }), notes: [] } });
  const phase = stagePhase(s);
  assert.equal(phase.kind, 'confirmed');
  assert.equal(phase.title, 'Waiting for your approval');
  assert.doesNotMatch(phase.text, /approved\)|ended/);
});

test('a degraded session shows Preview down with its URL, and clears when degraded goes false', () => {
  let s = reduce(initialState('i'), { type: 'view', view: { session: session({ degraded: true, preview_url: 'http://127.0.0.1:5173/' }), notes: [] } });
  s = reduce(s, { type: 'event', event: { event: 'frame', seq: 1, w: 800, h: 500, at: 'x' } });
  const phase = stagePhase(s);
  assert.equal(phase.kind, 'preview_down');
  assert.match(phase.text, /127\.0\.0\.1:5173/);
  assert.equal(s.degraded, true, 'a stale frame does not clear it');
  s = reduce(s, { type: 'event', event: { event: 'session', state: 'live', degraded: false, preview_path: '/', commit: null, browser: { status: 'running' }, note_version: 3 } });
  assert.equal(stagePhase(s).kind, 'live');
});

test('the current page follows the session events', () => {
  let s = reduce(initialState('i'), { type: 'view', view: { session: session({ preview_url: 'http://x.test/' }), notes: [] } });
  assert.equal(currentPath(s), '/');
  s = reduce(s, { type: 'event', event: { event: 'session', state: 'live', degraded: false, preview_path: '/about', commit: null, browser: { status: 'running' }, note_version: 3 } });
  assert.equal(currentPath(s), '/about');
});
