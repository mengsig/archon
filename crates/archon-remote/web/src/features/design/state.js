// The design screen's state as a pure reducer, and what the state means for
// the screen: which of the sec 7.3 states shows, and how a note reads.
import { DROP_OFFERED_AFTER_MINUTES } from '../../core/design.js';

export function initialState(item) {
  return {
    item, loaded: false, observer: null, view: null, session: null, notes: [], browser: { status: 'idle' },
    degraded: false, noteVersion: 0, frame: null, selection: null, blocked: [], progress: '',
    dialog: null, toasts: [], ended: null, connection: 'open', mode: 'interact', preset: 'desktop', refresh: false, error: null,
    previewCommit: null, path: null, finishBlocker: null, imagesUsed: 0, imagesMax: 0, deadline: null,
    itemState: null,
  };
}

const byNumber = (a, b) => b.number - a.number;

export function pathOf(url) {
  if (!url) return null;
  try { return new URL(url, 'http://x.invalid').pathname || '/'; } catch { return null; }
}

// The page the preview shows now: the last path the daemon reported, else the session's URL.
export function currentPath(state) {
  return state.path ?? pathOf(state.session?.preview_url) ?? '';
}

export function upsertNote(notes, note) {
  if (!note?.id) return notes;
  const rest = notes.filter((n) => n.id !== note.id);
  return [...rest, note].sort(byNumber);
}

export function reduce(state, action) {
  switch (action.type) {
    case 'view': {
      const v = action.view;
      // The daemon may answer with no session once the old one is gone; the screen keeps showing the one it had, as ended.
      if (!v.session && state.session) {
        return { ...state, loaded: true, observer: null, view: v, notes: [...(v.notes ?? [])].sort(byNumber), refresh: false, error: null,
          session: { ...state.session, state: 'ended' }, ended: state.ended ?? 'ended' };
      }
      // A different session id is a new session: nothing of the old picture or selection carries over.
      const fresh = Boolean(state.session && v.session && v.session.id !== state.session.id);
      return {
        ...state, loaded: true, observer: null, view: v, session: v.session ?? null, notes: [...(v.notes ?? [])].sort(byNumber),
        browser: v.browser ?? state.browser, degraded: Boolean(v.session?.degraded), noteVersion: v.session?.note_version ?? 0, path: pathOf(v.session?.preview_url) ?? state.path,
        preset: v.session?.viewport_preset ?? state.preset, finishBlocker: v.finish_blocker ?? null,
        imagesUsed: v.images_used ?? 0, imagesMax: v.images_max ?? 0, deadline: v.stage_deadline ?? null, refresh: false, error: null,
        ended: v.session && v.session.state === 'ended' ? (v.session.end_reason ?? 'ended') : null,
        ...(fresh ? { frame: null, selection: null, progress: '', previewCommit: null, blocked: [], mode: 'interact', toasts: [...state.toasts, 'A new design session started'] } : {}),
      };
    }
    case 'summary':
      return { ...state, loaded: true, observer: { state: action.summary.state ?? null, noteCount: action.summary.note_count ?? 0 }, error: null };
    case 'error':
      return { ...state, loaded: true, error: action.error };
    case 'connection':
      return { ...state, connection: action.state === 'open' ? 'open' : 'reconnecting' };
    // The item's own state (queued, running, succeeded…), from the app's item list.
    case 'item_state':
      return action.state === state.itemState ? state : { ...state, itemState: action.state ?? null };
    case 'mode':
      return { ...state, mode: action.mode, selection: action.mode === 'comment' || action.mode === 'region' ? state.selection : null };
    case 'preset':
      return { ...state, preset: action.preset };
    case 'selection':
      return { ...state, selection: action.selection };
    case 'toasts_taken':
      return { ...state, toasts: [] };
    case 'dialog_closed':
      return { ...state, dialog: null };
    case 'event':
      return reduceEvent(state, action.event);
    default:
      throw new RangeError(`no design action ${action.type}`);
  }
}

const specific = (reason) => (reason && reason !== 'ended' ? reason : null);

function reduceEvent(state, e) {
  switch (e.event) {
    case 'frame':
      return { ...state, frame: { seq: e.seq, w: e.w, h: e.h, at: e.at } };
    case 'selection':
      return { ...state, selection: { box: e.box, label: e.label } };
    case 'session': {
      const session = state.session ? { ...state.session, state: e.state, degraded: e.degraded, note_version: e.note_version } : state.session;
      const stale = e.note_version !== state.noteVersion && state.noteVersion !== 0;
      return { ...state, session, previewCommit: e.commit ?? state.previewCommit, path: e.preview_path ?? state.path, browser: e.browser ?? state.browser, degraded: e.degraded, noteVersion: e.note_version, refresh: state.refresh || stale || !state.session || (e.state === 'ended' && !(e.end_reason ?? e.reason) && !state.ended),
        ended: e.state === 'ended' ? (e.end_reason ?? e.reason ?? state.ended ?? 'ended') : state.ended };
    }
    case 'note':
      return { ...state, notes: upsertNote(state.notes, flat(e)) };
    case 'blocked_hosts':
      return { ...state, blocked: e.hosts ?? [] };
    case 'agent_progress':
      return { ...state, progress: e.line };
    case 'dialog':
      return { ...state, dialog: { kind: e.kind, message: e.message, defaultPrompt: e.default_prompt ?? null } };
    case 'toast':
      return { ...state, toasts: [...state.toasts, e.message] };
    case 'ended':
      // A bare "ended" (a gone refusal) must not wipe a reason already known; ask the daemon for the real one.
      return { ...state, ended: specific(e.reason) ?? state.ended ?? 'ended', refresh: state.refresh || (!specific(e.reason) && !state.ended), session: state.session ? { ...state.session, state: 'ended' } : state.session };
    default:
      return state;
  }
}

// A DesignNoteView is a DesignNote flattened with undone_by, read_only, active; the event nests nothing more.
function flat(e) {
  const { event, ...note } = e;
  return note;
}

// ---- what the state means ----

export function sessionState(state) {
  return state.session?.state ?? null;
}

export function isLive(state) {
  const s = sessionState(state);
  return s === 'starting' || s === 'live' || s === 'finishing';
}

// The one state that fills the stage; the banners and the status bar add to it.
export function stagePhase(state, { operator = true, features = true } = {}) {
  if (!features) return { kind: 'unavailable', title: 'Live design is not available', text: 'This host does not offer design sessions. Update Archon on the host to use them.', tone: 'muted' };
  if (state.error && !state.loaded) return { kind: 'error', title: 'Could not load the design session', text: state.error, tone: 'red' };
  if (!state.loaded) return { kind: 'loading', title: 'Loading…', text: '', tone: 'muted' };
  if (!operator || state.observer) {
    return { kind: 'observer', title: 'A design session is running', text: 'Only the operator’s devices see the live preview.', tone: 'muted' };
  }
  if (!state.session) return { kind: 'none', title: 'No design session', text: 'This item has no live design session yet. It opens when the agent has the app running.', tone: 'muted' };
  const s = state.session.state;
  if (s === 'confirmed' && !state.ended) {
    return { kind: 'confirmed', title: 'Waiting for your approval', text: 'You confirmed the design. Approve it, ask for changes, or reject it.', tone: 'accent' };
  }
  if (state.ended || s === 'ended') {
    const end = endInfo(state.ended ?? state.session.end_reason, state.itemState);
    return { kind: 'ended', end: end.end, title: end.title, text: end.text, tone: end.tone };
  }
  const b = state.browser?.status;
  if (b === 'missing') return { kind: 'no_browser', title: 'No browser on this machine', text: 'Archon needs Chrome or Chromium on this machine to show the preview.', detail: state.browser.hint ?? '', tone: 'amber' };
  if (b === 'sandbox_unavailable') return { kind: 'sandbox', title: 'Sandbox unavailable', text: 'Chromium’s sandbox is unavailable on this host.', detail: state.browser.hint ?? '', tone: 'amber' };
  if (b === 'crashed_too_often') return { kind: 'preview_down', title: 'The preview browser keeps stopping', text: 'The browser crashed too often. Reload, or tell the agent again.', tone: 'red' };
  if (b === 'queued_for_slot') return { kind: 'queued_slot', title: 'Queued for a preview slot', text: 'Another design session is using the host’s preview slots. Yours starts as soon as one is free.', tone: 'amber' };
  if (state.connection === 'reconnecting') return { kind: 'reconnecting', title: 'Reconnecting…', text: 'Notes you type meanwhile are kept on this device and sent when the connection is back.', tone: 'amber' };
  if (s === 'starting' || b === 'starting' || !state.frame) {
    return { kind: 'starting', title: 'Starting your app…', text: state.progress || (s === 'starting' ? 'Waiting for the agent to run the app.' : 'Waiting for the first picture…'), tone: 'muted' };
  }
  if (state.degraded) {
    const url = state.session?.preview_url;
    return { kind: 'preview_down', retry: true, title: 'Preview down', text: `${url ? `${url} is` : 'The app is'} not responding. The agent has been told and is restarting it. The picture behind this is the last one received.`, tone: 'red' };
  }
  if (s === 'finishing') return { kind: 'finishing', title: 'Finishing', text: 'Finishing: choose Review to continue, or Back to editing to change anything.', tone: 'accent' };
  return { kind: 'live', title: 'Live', text: '', tone: 'green' };
}

// What each way a session ends means for the operator, from the daemon's end_reason
// (the gate's answers are approved / revised / rejected).
// An approved design says what became of the item once it settled
// (`itemState` is the item's state: succeeded, failed, cancelled or still going).
export function endInfo(reason, itemState = null) {
  const r = String(reason ?? '').toLowerCase();
  if (/approv/.test(r)) {
    if (itemState === 'succeeded') return { end: 'approved', title: 'Design approved and built', text: 'The item is done: the approved design is in its result. Open the item to see it.', dot: 'Built', tone: 'green' };
    if (itemState === 'failed') return { end: 'approved', title: 'Design approved', text: 'The item stopped after the design was approved. Open the item to see what happened.', dot: 'Approved', tone: 'amber' };
    if (itemState === 'cancelled') return { end: 'approved', title: 'Design approved', text: 'The item was cancelled after the design was approved.', dot: 'Approved', tone: 'muted' };
    return { end: 'approved', title: 'Design approved', text: 'The agent is building it now. Follow its progress on the item.', dot: 'Approved', tone: 'green' };
  }
  if (/revis|change/.test(r)) return { end: 'revised', title: 'Changes requested', text: 'A new design session starts with your changes. This screen switches to it when it opens.', dot: 'Changes requested', tone: 'accent' };
  if (/reject/.test(r)) return { end: 'rejected', title: 'Design rejected', text: 'The item stops here.', dot: 'Rejected', tone: 'red' };
  if (/cancel/.test(r)) return { end: 'cancelled', title: 'Session cancelled', text: 'The design session was cancelled. Open the item to see where it stands.', dot: 'Cancelled', tone: 'muted' };
  if (/fail/.test(r)) return { end: 'failed', title: 'Session failed', text: 'The design session failed. Open the item to see what happened.', dot: 'Failed', tone: 'red' };
  if (/time/.test(r)) return { end: 'timed_out', title: 'Session timed out', text: 'The design session ran out of time. Open the item to continue.', dot: 'Timed out', tone: 'amber' };
  if (/complet/.test(r)) return { end: 'completed', title: 'Session completed', text: 'The design session is complete. Open the item to see the result.', dot: 'Completed', tone: 'muted' };
  return { end: 'ended', title: 'This design session ended', text: 'Open the item to see where it stands.', dot: 'Ended', tone: 'muted' };
}

// The ledger's lede after Confirm. The approval question opens once the agent
// has wrapped up (and arrives a moment after each load), so its absence is no
// sign that nothing is open.
export function ledgerLede(question, canOperate = true) {
  if (!canOperate) return 'Here is what changed. An operator device approves it, asks for changes, or rejects it.';
  return question ? 'Here is what changed. Approve it, ask for changes, or reject it.'
    : 'The agent is wrapping up. Approve, Ask for changes and Reject appear here in a moment.';
}

// While the gate waits for an answer, or after "Ask for changes", the item's next session is on its way: poll until it opens.
export function followsNewSession(state) {
  const s = state.session?.state;
  if (!state.session) return false;
  if (s === 'confirmed' && !state.ended) return true;
  return (state.ended != null || s === 'ended') && endInfo(state.ended ?? state.session.end_reason).end === 'revised';
}

// The top bar's dot: what the phase is, or how the session ended.
export function dotLook(state) {
  const phase = stagePhase(state);
  if (phase.kind === 'ended') { const e = endInfo(state.ended ?? state.session?.end_reason, state.itemState); return { label: e.dot, tone: e.tone }; }
  const kind = phase.kind;
  if (kind === 'finishing') return { label: 'Finishing', tone: 'accent' };
  if (kind === 'live') return { label: 'Live', tone: 'green' };
  if (kind === 'confirmed') return { label: 'Awaiting approval', tone: 'accent' };
  if (kind === 'preview_down') return { label: 'Preview down', tone: 'red' };
  if (kind === 'reconnecting') return { label: 'Reconnecting', tone: 'amber' };
  if (kind === 'starting') return { label: 'Starting', tone: 'muted' };
  return { label: 'Offline', tone: 'muted' };
}

// ---- viewport presets, confirmed by the stream ----

// The daemon's CSS-pixel sizes; a frame carries the size it was streamed at.
export const PRESET_SIZE = { desktop: { w: 1440, h: 900 }, tablet: { w: 834, h: 1112 }, phone: { w: 390, h: 844 } };
const PRESET_NAME = { desktop: 'Desktop', tablet: 'Tablet', phone: 'Phone' };
export const presetName = (p) => PRESET_NAME[p] ?? p;

// The preset the latest frame proves, or null before a frame or for an unknown shape.
// By shape, not exact size: on a slow link the daemon streams frames scaled down
// (half, quarter…), and those still prove the preset. The three shapes are far apart.
export function confirmedPreset(frame) {
  if (!frame || !(frame.w > 0) || !(frame.h > 0)) return null;
  const ratio = frame.w / frame.h;
  return Object.keys(PRESET_SIZE).find((k) => {
    const p = PRESET_SIZE[k];
    return frame.w <= p.w + 1 && Math.abs(ratio - p.w / p.h) < 0.02;
  }) ?? null;
}

// True while a frame has arrived at a size other than the requested preset's.
export function presetMismatch(state) {
  return Boolean(state.frame) && confirmedPreset(state.frame) !== state.preset;
}

// Taps resolve against the streamed size, so they wait until it matches.
export function pointerBlockedText(state) {
  return presetMismatch(state) ? `Switching to ${presetName(state.preset)}…` : '';
}

export function notesHeading(notes) {
  return `Notes (${notes.length})`;
}

export function activeNote(notes) {
  return notes.find((n) => !n.read_only && (n.status === 'working' || n.status === 'sent')) ?? null;
}

export function waitingAnswer(notes) {
  return notes.find((n) => !n.read_only && n.status === 'needs_reply') ?? null;
}

export function queuedNotes(notes) {
  return notes.filter((n) => !n.read_only && n.status === 'queued' && n.kind !== 'revert');
}

// Text under the frame: the preview commit and what the agent is doing.
export function statusLine(state) {
  const busy = activeNote(state.notes);
  const asking = waitingAnswer(state.notes);
  const parts = [];
  if (asking) parts.push(`Waiting for your answer on ${asking.id}`);
  else if (busy) parts.push(`Agent is working on ${busy.id}. New notes wait in line.`);
  if (state.progress && !asking) parts.push(`agent: ${state.progress}`);
  return parts.join(' · ');
}

// ---- notes ----

const FAIL_TEXT = {
  history_changed: 'The code history changed in a way Archon can’t track; its edits may still show. Add a note if something should change.',
  undo_conflict: 'Can’t undo automatically, later changes touch the same code; add a note describing what to change back.',
  undo_interrupted: 'Undo was interrupted; check the preview.',
  undo_refused: 'Archon refused to undo this change; add a note describing what to change back.',
  agent: 'The agent could not finish this note.',
  not_delivered: 'This note never reached the agent.',
};

export function failText(note) {
  if (note.fail_reason === 'undo_interrupted') return `Undo of ${note.reverts ?? 'the note'} was interrupted; check the preview.`;
  return FAIL_TEXT[note.fail_reason] ?? (note.status === 'failed' ? 'This note failed.' : '');
}

// Status chips pair an icon with a word, never colour alone.
export function noteLook(note) {
  if (note.undone_by) return { label: 'Undone', tone: 'muted', icon: 'undo' };
  if (note.status === 'failed' && note.fail_reason === 'not_delivered') return { label: 'Not delivered', tone: 'red', icon: 'alert' };
  switch (note.status) {
    case 'queued': return { label: 'Queued', tone: 'neutral', icon: 'clock' };
    case 'sent': case 'working': return { label: 'Working', tone: 'accent', icon: 'working' };
    case 'needs_reply': return { label: 'Needs reply', tone: 'amber', icon: 'help' };
    case 'done': return { label: 'Done', tone: 'green', icon: 'check' };
    case 'failed': return { label: 'Failed', tone: 'red', icon: 'alert' };
    case 'cancelled': return { label: 'Cancelled', tone: 'muted', icon: 'stop' };
    default: return { label: note.status, tone: 'neutral', icon: 'clock' };
  }
}

export function minutesSince(iso, now = Date.now()) {
  const t = Date.parse(iso);
  return Number.isFinite(t) ? (now - t) / 60000 : 0;
}

// What the operator may do with a note, as booleans; the daemon still decides.
export function noteActions(note, { finishing = false, live = false, now = Date.now() } = {}) {
  const none = { edit: false, cancel: false, reply: false, drop: false, undo: false, compare: false, resend: false };
  // An earlier session's note may be undone while the current session is live (the daemon decides), unless approved.
  if (note.read_only) return { ...none, compare: hasPictures(note), undo: live && !finishing && canUndo(note) && !note.approved };
  const compare = hasPictures(note);
  if (finishing) return { ...none, compare };
  const out = { ...none, compare };
  if (note.kind === 'revert') return out;
  if (note.status === 'queued') { out.edit = true; out.cancel = true; }
  if (note.status === 'needs_reply') { out.reply = true; out.drop = true; }
  if ((note.status === 'working' || note.status === 'sent') && minutesSince(note.updated_at, now) >= DROP_OFFERED_AFTER_MINUTES) out.drop = true;
  if (canUndo(note)) out.undo = true;
  if (note.status === 'failed' && note.fail_reason === 'not_delivered') out.resend = true;
  return out;
}

const canUndo = (note) => note.status === 'done' && Boolean(note.commit) && !note.undone_by && !note.no_change;

export function hasPictures(note) {
  return Boolean(note.before_asset && afterAssetOf(note));
}

// The picture taken after the change: the note's newest after capture (a
// note done again replaces it). Finish's final picture only stands in when no
// after capture was kept.
export function afterAssetOf(note) {
  return note.after_asset || (note.final_included !== false && note.final_asset) || null;
}

export function shortSha(sha) {
  return typeof sha === 'string' ? sha.slice(0, 7) : '';
}

export function captionFor(commit, at) {
  const time = at ? new Date(at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : '';
  return `captured from the preview at commit ${shortSha(commit) || 'unknown'}${time ? `, ${time}` : ''}`;
}

// `shown` is the session on screen: an ended one is shown as it ended, and its
// own notes are read-only because it ended, not because they are earlier.
export function sessionLabel(note, shown = null) {
  if (!note.read_only) return '';
  return shown && note.session === shown ? '' : 'Earlier session';
}

export function earlierText(note, shown = null) {
  if (!note.read_only) return '';
  if (shown && note.session === shown) return 'This session has ended: its notes can’t change.';
  return note.approved ? 'From an earlier session and approved: it can’t be edited or undone.' : 'From an earlier session: it can’t be edited. A done change can still be undone.';
}

export function imageCapWarning(used, max, included, margin = 8) {
  if (!max) return null;
  const total = used + included;
  if (total > max) return `This would use ${total} of ${max} pictures the item can hold. Switch some pictures off.`;
  if (max - total <= margin) return `${total} of ${max} pictures are used; only ${max - total} more fit.`;
  return null;
}
