// The design_* requests with the exact serde shapes of design_protocol, the
// reply kinds they answer with, and the stream's event hub.
import { DaemonError } from './transport.js';

export const DESIGN_FEATURE = 'design_sessions';
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
    asset: (item, id) => request('design_asset', { item, id }).then(expect('design_asset')),
    ack: (sub, seq) => request('design_frame_ack', { sub, seq }).then(expect('design_done')),
    input: (session, events) => request('design_input', { session, events }).then(expect('design_done')),
    resolve: (session, x, y, depth = 0, select = false) => request('design_resolve', { session, x, y, depth, select }).then(expect('design_resolved')),
    region: (session, box) => request('design_region', { session, box }).then(expect('design_resolved')),
    upload: (fields) => request('design_upload', { blank: false, ...fields }, { timeoutMs: 60_000 }).then(expect('design_resolved', 'design_asset')),
    viewport: (session, preset) => request('design_viewport', { session, preset }).then(expect('design_done')),
    reload: (session) => request('design_reload', { session }).then(expect('design_done')),
    tellAgent: (session) => request('design_tell_agent', { session }).then(expect('design_agent')),
    dialogAnswer: (session, accept, text = null) => request('design_dialog_answer', { session, accept, text }).then(expect('design_done')),
    noteAdd: (fields) => request('design_note_add', fields).then(expect('design_note')),
    noteEdit: (item, id, text) => request('design_note_edit', { item, id, text }).then(expect('design_note')),
    noteCancel: (item, id) => request('design_note_cancel', { item, id }).then(expect('design_note')),
    noteReply: (item, id, text) => request('design_note_reply', { item, id, text }).then(expect('design_note')),
    noteDrop: (item, id) => request('design_note_drop', { item, id }).then(expect('design_note')),
    noteUndo: (item, id) => request('design_note_undo', { item, id }).then(expect('design_note')),
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

export function designErrorText(err) {
  return REFUSALS[err?.code] ?? err?.message ?? 'Something went wrong.';
}
