// The notes panel: status chips, the thread, reply, edit, cancel, drop, undo,
// send again and compare. Earlier sessions' notes are read-only.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { reconcile } from '../../ui/list.js';
import { confirmSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { noteLook, noteActions, failText, shortSha, sessionLabel, earlierText, notesHeading } from './state.js';
import { openCompare } from './compare.js';
import { designErrorText, MAX_REPLY_CHARS, MAX_NOTE_CHARS } from '../../core/design.js';

export function statusChip(note) {
  const look = noteLook(note);
  return h('span', { class: ['chip', `tone-${look.tone}`, 'note-chip'] }, icon(look.icon, { size: 14 }), h('span', { class: 'chip-text', text: look.label }));
}

export function createNotesPanel({ ctx, getState, onFocusNote, onAddPage }) {
  const list = h('ul', { class: 'note-list', 'aria-label': 'Notes' });
  const empty = emptyNotes();
  const pending = h('ul', { class: 'note-list note-pending', 'aria-label': 'Notes waiting to be sent' });
  const add = h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => onAddPage() }, icon('plus', { size: 16 }), ' Note on the whole page');
  const title = h('h2', { class: 'notes-title', id: 'design-notes-title', text: 'Notes' });
  const el = h('section', { class: 'notes', 'aria-labelledby': 'design-notes-title' }, h('div', { class: 'notes-head' }, title, add), pending, list, empty);
  let selected = null;

  const act = async (fn, okText) => {
    try {
      await fn();
      if (okText) toast(okText, { tone: 'green' });
    } catch (err) {
      toast(designErrorText(err), { tone: 'red' });
    }
  };

  function card(note) {
    const state = getState();
    const acts = noteActions(note, { finishing: state.session?.state === 'finishing', live: state.session?.state === 'live' });
    const look = noteLook(note);
    const li = h('li', { class: ['note', `status-${look.label.toLowerCase().replace(/ /g, '-')}`, note.read_only && 'is-readonly', selected === note.id && 'is-selected'], id: `note-${note.id}`, tabindex: '-1' });
    li.append(h('div', { class: 'note-head' }, h('strong', { class: 'note-id', text: note.id }), statusChip(note),
      sessionLabel(note, state.session?.id) ? h('span', { class: 'chip tone-neutral note-chip note-earlier' }, h('span', { class: 'chip-text', text: sessionLabel(note, state.session?.id) })) : null,
      note.kind === 'revert' ? h('span', { class: 'note-tag', text: `Undo of ${note.reverts ?? ''}`.trim() }) : null,
      note.resent_from ? h('span', { class: 'note-tag', text: `Re-sent from ${note.resent_from}` }) : null));
    li.append(h('p', { class: 'note-text', text: note.text || (note.kind === 'revert' ? 'Reverse this change' : '') }));
    if (note.read_only) li.append(h('p', { class: 'note-hint', text: earlierText(note, state.session?.id) }));
    if (note.undone_by) li.append(h('p', { class: 'note-hint', text: `Undone by ${note.undone_by}. It no longer counts.` }));
    if (note.no_change) li.append(h('p', { class: 'note-hint', text: 'Done, nothing to change.' }));
    if (note.status === 'done' && note.commit) li.append(h('p', { class: 'note-hint', text: `Commit ${shortSha(note.commit)}` }));
    const fail = failText(note);
    if (note.status === 'failed' && fail) li.append(h('p', { class: 'note-fail', role: 'note', text: fail }));
    // A failed note says why once, in the red box; Archon's own failure reply would repeat it.
    const thread = (note.thread ?? []).filter((t) => !(note.status === 'failed' && fail && t.from === 'archon'));
    if (thread.length) {
      li.append(h('ol', { class: 'note-thread', 'aria-label': `Thread on ${note.id}` }, thread.map((t) => h('li', { class: ['thread-entry', `from-${t.from}`] },
        h('span', { class: 'thread-from', text: { operator: 'You', agent: 'Agent', archon: 'Archon' }[t.from] ?? t.from }), h('span', { text: t.text })))));
    }
    const actions = h('div', { class: 'note-actions' });
    if (acts.compare) actions.append(button('Compare', 'compare', () => openCompare(ctx, note)));
    if (acts.edit) actions.append(button('Edit', 'pencil', () => editNote(li, note)));
    if (acts.cancel) actions.append(button('Cancel', 'close', () => act(() => ctx.design.noteCancel(note.item, note.id), `${note.id} cancelled`)));
    if (acts.drop) actions.append(button('Drop note', 'stop', () => dropNote(note)));
    if (acts.undo) actions.append(button('Undo', 'undo', () => undoNote(note)));
    if (acts.resend) actions.append(button('Send again', 'retry', () => act(() => ctx.design.noteResend(note.item, note.id), `${note.id} sent again as a new note`)));
    if (actions.children.length) li.append(actions);
    if (acts.reply) li.append(replyForm(note));
    return li;
  }

  async function undoNote(note) {
    const ok = await confirmSheet({ title: `Undo ${note.id}?`, message: `Undo ${note.id}? Archon will reverse its change.`, confirmLabel: `Undo ${note.id}`, cancelLabel: 'Keep it', tone: 'primary' });
    if (ok) act(() => ctx.design.noteUndo(note.item, note.id), `Undo of ${note.id} queued`);
  }
  async function dropNote(note) {
    const ok = await confirmSheet({ title: `Drop ${note.id}?`, message: `The agent discards its unfinished edit for ${note.id} and the queue moves on.`, confirmLabel: 'Drop note', cancelLabel: 'Keep waiting' });
    if (ok) act(() => ctx.design.noteDrop(note.item, note.id), `${note.id} dropped`);
  }
  function editNote(li, note) {
    const box = h('textarea', { class: 'input textarea', rows: '3', maxlength: String(MAX_NOTE_CHARS), 'aria-label': `Edit ${note.id}`, value: note.text });
    const save = h('button', { class: 'button button-primary button-small', type: 'submit' }, 'Save');
    const stop = h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => form.replaceWith() }, 'Cancel edit');
    const form = h('form', { class: 'note-edit', novalidate: true }, box, h('div', { class: 'note-actions' }, save, stop));
    form.addEventListener('submit', (e) => { e.preventDefault(); if (box.value.trim()) act(() => ctx.design.noteEdit(note.item, note.id, box.value.trim())); });
    li.append(form);
    box.focus();
  }
  function replyForm(note) {
    const box = h('textarea', { class: 'input textarea', rows: '2', maxlength: String(MAX_REPLY_CHARS), 'aria-label': `Reply to ${note.id}`, placeholder: 'Answer the agent' });
    const form = h('form', { class: 'note-reply', novalidate: true }, box, h('button', { class: 'button button-primary button-small', type: 'submit' }, 'Reply'));
    form.addEventListener('submit', (e) => { e.preventDefault(); if (box.value.trim()) act(() => ctx.design.noteReply(note.item, note.id, box.value.trim())); });
    return form;
  }
  function button(label, iconName, onclick) {
    return h('button', { class: 'button button-plain button-small', type: 'button', onclick }, icon(iconName, { size: 16 }), ` ${label}`);
  }

  function render() {
    const sessionState = getState().session?.state;
    add.hidden = sessionState === 'confirmed' || sessionState === 'ended';
    const state = getState();
    const notes = state.notes;
    reconcile(list, notes, {
      key: (n) => n.id,
      signature: (n) => { const acts = noteActions(n, { finishing: state.session?.state === 'finishing', live: state.session?.state === 'live' }); return JSON.stringify([n.approved, n.status, n.text, n.thread, n.undone_by, n.fail_reason, n.read_only, n.after_asset, n.commit, state.session?.state, selected === n.id, acts.drop, acts.undo]); },
      render: card,
    });
    empty.hidden = notes.length > 0 || pending.children.length > 0;
    title.textContent = notesHeading(notes);
    add.disabled = state.session?.state !== 'live';
  }

  return {
    element: el, render,
    // Notes typed while offline that have not reached the daemon yet.
    setPending(entries) {
      pending.replaceChildren(...entries.map((e) => h('li', { class: 'note note-unsent' },
        h('div', { class: 'note-head' }, h('span', { class: 'chip tone-amber note-chip' }, icon('clock', { size: 14 }), h('span', { class: 'chip-text', text: 'Waiting to send' }))),
        h('p', { class: 'note-text', text: e.text }))));
      render();
    },
    select(id) {
      selected = id;
      render();
      const target = el.querySelector(`#note-${CSS.escape(id)}`);
      target?.scrollIntoView?.({ block: 'nearest' });
      target?.focus({ preventScroll: true });
      onFocusNote(id);
    },
    get selected() { return selected; },
  };
}

// Three steps drawn with CSS shapes, not images.
function emptyNotes() {
  const step = (n, text) => h('li', { class: 'empty-step' }, h('span', { class: ['empty-shape', `shape-${n}`], 'aria-hidden': 'true' }), h('span', { text }));
  return h('div', { class: 'notes-empty' },
    h('p', { class: 'notes-empty-text', text: 'Click Comment, then click anything in your app to ask for a change.' }),
    h('ol', { class: 'empty-steps' }, step(1, 'Press Comment'), step(2, 'Click what to change'), step(3, 'Say it in plain words')));
}
