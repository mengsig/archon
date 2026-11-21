// The design screen's state as a pure reducer, and what the state means for
// the screen: which of the sec 7.3 states shows, and how a note reads.
import { DROP_OFFERED_AFTER_MINUTES } from '../../core/design.js';

export function initialState(item) {
  return {
    item, loaded: false, observer: null, view: null, session: null, notes: [], browser: { status: 'idle' },
    degraded: false, noteVersion: 0, frame: null, selection: null, blocked: [], progress: '',
    dialog: null, toasts: [], ended: null, connection: 'open', mode: 'interact', preset: 'desktop', refresh: false, error: null,
    previewCommit: null, finishBlocker: null, imagesUsed: 0, imagesMax: 0, deadline: null,
  };
}

const byNumber = (a, b) => b.number - a.number;

export function upsertNote(notes, note) {
  if (!note?.id) return notes;
  const rest = notes.filter((n) => n.id !== note.id);
  return [...rest, note].sort(byNumber);
}

export function reduce(state, action) {
  switch (action.type) {
    case 'view': {
      const v = action.view;
      return {
        ...state, loaded: true, observer: null, view: v, session: v.session ?? null, notes: [...(v.notes ?? [])].sort(byNumber),
        browser: v.browser ?? state.browser, degraded: Boolean(v.session?.degraded), noteVersion: v.session?.note_version ?? 0,
        preset: v.session?.viewport_preset ?? state.preset, finishBlocker: v.finish_blocker ?? null,
        imagesUsed: v.images_used ?? 0, imagesMax: v.images_max ?? 0, deadline: v.stage_deadline ?? null, refresh: false, error: null,
        ended: v.session && v.session.state === 'ended' ? (v.session.end_reason ?? 'ended') : null,
      };
    }
    case 'summary':
      return { ...state, loaded: true, observer: { state: action.summary.state ?? null, noteCount: action.summary.note_count ?? 0 }, error: null };
    case 'error':
      return { ...state, loaded: true, error: action.error };
    case 'connection':
      return { ...state, connection: action.state === 'open' ? 'open' : 'reconnecting' };
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

function reduceEvent(state, e) {
  switch (e.event) {
    case 'frame':
      return { ...state, frame: { seq: e.seq, w: e.w, h: e.h, at: e.at }, degraded: false };
    case 'selection':
      return { ...state, selection: { box: e.box, label: e.label } };
    case 'session': {
      const session = state.session ? { ...state.session, state: e.state, degraded: e.degraded, note_version: e.note_version } : state.session;
      const stale = e.note_version !== state.noteVersion && state.noteVersion !== 0;
      return { ...state, session, previewCommit: e.commit ?? state.previewCommit, browser: e.browser ?? state.browser, degraded: e.degraded, noteVersion: e.note_version, refresh: state.refresh || stale || !state.session,
        ended: e.state === 'ended' ? (state.ended ?? 'ended') : state.ended };
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
      return { ...state, ended: e.reason || 'ended', session: state.session ? { ...state.session, state: 'ended' } : state.session };
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
  if (state.ended || s === 'ended' || s === 'confirmed') {
    return { kind: 'ended', title: 'This design session ended', text: endedText(state.ended ?? state.session.end_reason, s), tone: 'muted' };
  }
  const b = state.browser?.status;
  if (b === 'missing') return { kind: 'no_browser', title: 'No browser on this machine', text: `Archon needs Chrome or Chromium on this machine to show the preview. ${state.browser.hint ?? ''}`.trim(), tone: 'amber' };
  if (b === 'sandbox_unavailable') return { kind: 'sandbox', title: 'Sandbox unavailable', text: `Chromium’s sandbox is unavailable on this host. ${state.browser.hint ?? ''}`.trim(), tone: 'amber' };
  if (b === 'crashed_too_often') return { kind: 'preview_down', title: 'The preview browser keeps stopping', text: 'The browser crashed too often. Reload, or tell the agent again.', tone: 'red' };
  if (b === 'queued_for_slot') return { kind: 'queued_slot', title: 'Queued for a preview slot', text: 'Another design session is using the host’s preview slots. Yours starts as soon as one is free.', tone: 'amber' };
  if (state.connection === 'reconnecting') return { kind: 'reconnecting', title: 'Reconnecting…', text: 'Notes you type meanwhile are kept on this device and sent when the connection is back.', tone: 'amber' };
  if (s === 'starting' || b === 'starting' || !state.frame) {
    return { kind: 'starting', title: 'Starting your app…', text: state.progress || (s === 'starting' ? 'Waiting for the agent to run the app.' : 'Waiting for the first picture…'), tone: 'muted' };
  }
  if (state.degraded) return { kind: 'preview_down', title: 'The app stopped responding', text: 'The agent has been told and is restarting it.', tone: 'red' };
  if (s === 'finishing') return { kind: 'finishing', title: 'Finishing', text: 'Finishing: press Back to editing to change anything.', tone: 'accent' };
  return { kind: 'live', title: 'Live', text: '', tone: 'green' };
}

function endedText(reason, state) {
  const r = String(reason ?? '');
  if (/approv/i.test(r) || state === 'confirmed') return 'This design session ended (approved).';
  if (/revis/i.test(r)) return 'This design session ended (revised).';
  if (/cancel/i.test(r)) return 'This design session ended (cancelled).';
  return r && r !== 'ended' ? `This design session ended (${r}).` : 'This design session ended.';
}

// The top bar's dot: Live (green) or Finishing (blue); nothing for other phases.
export function dotLook(state) {
  const phase = stagePhase(state).kind;
  if (phase === 'finishing') return { label: 'Finishing', tone: 'accent' };
  if (phase === 'live') return { label: 'Live', tone: 'green' };
  if (phase === 'preview_down') return { label: 'Preview down', tone: 'red' };
  if (phase === 'reconnecting') return { label: 'Reconnecting', tone: 'amber' };
  if (phase === 'starting') return { label: 'Starting', tone: 'muted' };
  return { label: phase === 'ended' ? 'Ended' : 'Offline', tone: 'muted' };
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
export function noteActions(note, { finishing = false, now = Date.now() } = {}) {
  const none = { edit: false, cancel: false, reply: false, drop: false, undo: false, compare: false, resend: false };
  if (note.read_only) return { ...none, compare: hasPictures(note), resend: false };
  const compare = hasPictures(note);
  if (finishing) return { ...none, compare };
  const out = { ...none, compare };
  if (note.kind === 'revert') return out;
  if (note.status === 'queued') { out.edit = true; out.cancel = true; }
  if (note.status === 'needs_reply') { out.reply = true; out.drop = true; }
  if ((note.status === 'working' || note.status === 'sent') && minutesSince(note.updated_at, now) >= DROP_OFFERED_AFTER_MINUTES) out.drop = true;
  if (note.status === 'done' && note.commit && !note.undone_by && !note.no_change) out.undo = true;
  if (note.status === 'failed' && note.fail_reason === 'not_delivered') out.resend = true;
  return out;
}

export function hasPictures(note) {
  return Boolean(note.before_asset && note.after_asset);
}

export function shortSha(sha) {
  return typeof sha === 'string' ? sha.slice(0, 7) : '';
}

export function captionFor(commit, at) {
  const time = at ? new Date(at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' }) : '';
  return `captured from the preview at commit ${shortSha(commit) || 'unknown'}${time ? `, ${time}` : ''}`;
}

export function sessionLabel(note) {
  return note.read_only ? 'Earlier session' : '';
}

export function imageCapWarning(used, max, included, margin = 8) {
  if (!max) return null;
  const total = used + included;
  if (total > max) return `This would use ${total} of ${max} pictures the item can hold. Switch some pictures off.`;
  if (max - total <= margin) return `${total} of ${max} pictures are used; only ${max - total} more fit.`;
  return null;
}
