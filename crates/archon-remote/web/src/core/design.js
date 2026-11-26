// The design_* requests with the exact serde shapes of design_protocol, the
// reply kinds they answer with, and the stream's event hub.
import { DaemonError } from './transport.js';

export const DESIGN_FEATURE = 'design_sessions';
export const ITERATIONS_FEATURE = 'design_iterations';
export const MAX_REFS = 5;
export const MAX_VERSIONS = 20;
export const MAX_NOTE_CHARS = 4000;
export const MAX_REPLY_CHARS = 2000;
export const MAX_DETAILS_CHARS = 2000;
export const MAX_INPUT_EVENTS = 64;
export const MAX_ENCODED_IMAGE_BYTES = 560 * 1024;
export const MAX_IMAGE_SIDE = 4096;
export const IMAGE_CAP_WARN_MARGIN = 8;
export const DROP_OFFERED_AFTER_MINUTES = 15;

function expect(...kinds) {
  return (reply) => {
    if (!kinds.includes(reply?.reply)) throw new DaemonError('internal', `expected a ${kinds.join(' or ')} reply, got ${reply?.reply}`);
    return reply.data;
  };
}

export function hasDesign(welcome) {
  return Array.isArray(welcome?.features) && welcome.features.includes(DESIGN_FEATURE);
}

export function hasIterations(welcome) {
  return Array.isArray(welcome?.features) && welcome.features.includes(ITERATIONS_FEATURE);
}

export function designCommands(request) {
  if (typeof request !== 'function') throw new TypeError('design commands need a request function');
  return {
    // An operator gets the full state; an observer a one-line summary.
    get: (item) => request('design_get', { item }).then((reply) => ({ kind: reply?.reply, data: expect('design_state', 'design_summary')(reply) })),
    subscribe: (session) => request('subscribe_design', { session }).then((reply) => {
      const data = expect('subscribed')(reply);
      if (!Number.isInteger(data?.id)) throw new DaemonError('internal', 'subscribe_design answered without a sub id');
      return data.id;
    }),
    unsubscribe: (sub) => request('design_unsubscribe', { sub }).then(expect('design_done')),
    asset: (item, id) => request('design_asset', { item, id }).then(expect('design_asset')),
    ack: (sub, seq) => request('design_frame_ack', { sub, seq }).then(expect('design_done')),
    input: (session, events) => request('design_input', { session, events }, { timeoutMs: INPUT_TIMEOUT_MS }).then(expect('design_done')),
    resolve: (session, x, y, depth = 0, select = false) => request('design_resolve', { session, x, y, depth, select }).then(expect('design_resolved')),
    region: (session, box) => request('design_region', { session, box }).then(expect('design_resolved')),
    upload: (fields) => request('design_upload', { blank: false, ...fields }, { timeoutMs: 60_000 }).then(expect('design_resolved', 'design_asset')),
    viewport: (session, preset) => request('design_viewport', { session, preset }).then(expect('design_done')),
    filePart: (fields) => request('design_file_part', fields, { timeoutMs: 60_000 }).then(expect('design_done')),
    fileSet: (session, chooser, transfer) => request('design_file_set', { session, chooser, transfer }).then(expect('design_done')),
    fileDrop: (session, transfer, x, y) => request('design_file_drop', { session, transfer, x, y }).then(expect('design_done')),
    download: (session, id, offset) => request('design_download', { session, id, offset }).then(expect('design_download')),
    pickerAnswer: (session, id, value) => request('design_picker_answer', { session, id, value }).then(expect('design_done')),
    goBack: (session) => request('design_go_back', { session }).then(expect('design_done')),
    reload: (session) => request('design_reload', { session }).then(expect('design_done')),
    tellAgent: (session) => request('design_tell_agent', { session }).then(expect('design_agent')),
    dialogAnswer: (session, accept, text = null) => request('design_dialog_answer', { session, accept, text }).then(expect('design_done')),
    noteAdd: (fields) => request('design_note_add', fields).then(expect('design_note')),
    noteEdit: (item, id, text) => request('design_note_edit', { item, id, text }).then(expect('design_note')),
    noteCancel: (item, id) => request('design_note_cancel', { item, id }).then(expect('design_note')),
    noteReply: (item, id, text) => request('design_note_reply', { item, id, text }).then(expect('design_note')),
    noteDrop: (item, id) => request('design_note_drop', { item, id }).then(expect('design_note')),
    noteUndo: (item, id) => request('design_note_undo', { item, id }).then(expect('design_note')),
    // Reverts the chain's newer versions, then (with text) queues a follow-up on `id`.
    noteContinueFrom: (item, id, { text = null, clientKey = null, refs = [] } = {}) => request('design_note_continue_from', { item, id, text, client_key: clientKey, refs }).then(expect('design_note')),
    noteResend: (item, id) => request('design_note_resend', { item, id }).then(expect('design_note')),
    finishCapture: (session, { cancelQueued = false, recapture = false } = {}) =>
      request('design_finish_capture', { session, cancel_queued: cancelQueued, recapture }, { timeoutMs: 60_000 }).then(expect('design_finish')),
    finalCapture: (session, capture, note) => request('design_final_capture', { session, capture, note }).then(expect('design_finish', 'design_done')),
    finishConfirm: (session, { capture, include, superseded, confirmNoChange = false }) =>
      request('design_finish_confirm', { session, capture, include, superseded, confirm_no_change: confirmNoChange }).then(expect('design_done', 'design_state')),
    finishCancel: (session) => request('design_finish_cancel', { session }).then(expect('design_done')),
  };
}

// Stream events arrive as {frame:"design", sub, event:{event:"..."}}; the transport hands them here.
export function createDesignHub() {
  const listeners = new Set();
  return {
    push(sub, event) {
      if (!Number.isInteger(sub) || typeof event?.event !== 'string') throw new TypeError('a design frame has a sub and an event');
      for (const listener of [...listeners]) listener(sub, event);
    },
    subscribe(listener) {
      if (typeof listener !== 'function') throw new TypeError('a design listener is a function');
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

// Plain sentences for the refusals the design requests add.
const REFUSALS = {
  gone: 'This design session has ended. Open the item to see where it stands.',
  read_only: 'That note belongs to an earlier session and can no longer be changed.',
  finishing: 'Finishing: press Back to editing to change anything.',
  stale: 'The code changed since the pictures were captured.',
  conflict: 'This session already moved on (finished, cancelled or ended); the screen now shows where it is.',
};

// The daemon's error text starts with its error kind ("invalid argument: …");
// a person reads the sentence after it, capitalised and ended.
const KIND_PREFIX = /^(?:invalid argument|state transition refused|forbidden|unavailable|configuration|pipeline|protocol|recovering|store|corrupt record|aether|model policy|io):\s*/i;

export function plainMessage(text) {
  const rest = String(text ?? '').trim().replace(KIND_PREFIX, '').trim();
  if (!rest) return '';
  const said = rest[0].toUpperCase() + rest.slice(1);
  return /[.!?…]$/.test(said) ? said : `${said}.`;
}

// archond answers input within 20 s (READY_DEADLINE, INPUT_DEADLINE and RELEASE_DEADLINE);
// waiting for that, not a shorter guess, is what keeps one batch from overlapping the next.
export const INPUT_TIMEOUT_MS = 60_000;

// Input that may have reached the app only in part: say so, quietly.
export function isLostInput(err) {
  return err?.code === 'timeout'
    || (err?.code === 'unavailable' && /(may not have|did not) reach(ed)? the app/.test(String(err?.message ?? '')));
}

// archond refusing a request past its in-flight cap, or input past its rate:
// the app is ahead of the daemon. It waits, so these are rare and never shown as errors.
export function isBackpressure(err) {
  const message = String(err?.message ?? '');
  return (err?.code === 'unavailable' && /requests may be in flight/.test(message))
    || (err?.code === 'policy' && /too many input requests/.test(message));
}

export function designErrorText(err) {
  return REFUSALS[err?.code] ?? (plainMessage(err?.message) || 'Something went wrong.');
}
