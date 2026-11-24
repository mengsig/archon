// The notes panel: status chips, the thread, reply, edit, cancel, drop, undo,
// send again and compare. Earlier sessions' notes are read-only.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { reconcile } from '../../ui/list.js';
import { confirmSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { noteLook, noteActions, failText, shortSha, sessionLabel, earlierText, notesHeading } from './state.js';
import { openCompare } from './compare.js';
import { buildChains, chainOf, chainById, chainOfNote, followTarget, olderInCode, linkParts, versionLimitText, versionNumber, isSetAside } from './chains.js';
import { createFollowBox } from './followup.js';
import { designErrorText, MAX_REPLY_CHARS, MAX_NOTE_CHARS } from '../../core/design.js';

export function statusChip(note) {
  const look = noteLook(note);
  return h('span', { class: ['chip', `tone-${look.tone}`, 'note-chip'] }, icon(look.icon, { size: 14 }), h('span', { class: 'chip-text', text: look.label }));
}

// Text with the D<n> ids that are references drawn as links to their node.
export function linkedText(text, refs, onLink) {
  return linkParts(text, refs).map((p) => (p.ref
    ? h('button', { class: 'ref-link', type: 'button', 'aria-label': `Show ${p.ref} in the tree`, onclick: () => onLink(p.ref) }, p.ref)
    : p.text));
}

// A note's references its words do not already link, as links of their
// own, so one picked from the list without typing its id still shows.
export function refLinks(text, refs, onLink) {
  const inText = new Set(linkParts(text ?? '', refs ?? []).map((p) => p.ref).filter(Boolean));
  refs = (refs ?? []).filter((id) => !inText.has(id));
  if (!refs.length) return null;
  return h('p', { class: 'note-hint note-refs' }, 'References: ', ...refs.flatMap((id, i) => [i ? ' ' : null,
    h('button', { class: 'ref-link', type: 'button', 'aria-label': `Show ${id} in the tree`, onclick: () => onLink(id) }, id)]));
}

export function createNotesPanel({ ctx, getState, onFocusNote, onAddPage, iterations = () => false, thumbs = null, onLink = () => {}, onFollowUp = async () => {} }) {
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

  // Compare windows now open, by note: each is handed the note again on every render.
  const compares = new Map();
  function openLiveCompare(note) {
    const view = openCompare(ctx, note, { onClose: () => { if (compares.get(note.id) === view) compares.delete(note.id); } });
    compares.set(note.id, view);
  }

  function card(note) {
    const state = getState();
    const iterOn = iterations();
    const acts = noteActions(note, { finishing: state.session?.state === 'finishing', live: state.session?.state === 'live' });
    const look = noteLook(note);
    const li = h('li', { class: ['note', `status-${look.label.toLowerCase().replace(/ /g, '-')}`, note.read_only && 'is-readonly', selected === note.id && 'is-selected'], id: `note-${note.id}`, tabindex: '-1' });
    li.append(h('div', { class: 'note-head' }, h('strong', { class: 'note-id', text: note.id }), statusChip(note),
      sessionLabel(note, state.session?.id) ? h('span', { class: 'chip tone-neutral note-chip note-earlier' }, h('span', { class: 'chip-text', text: sessionLabel(note, state.session?.id) })) : null,
      note.kind === 'revert' ? h('span', { class: 'note-tag', text: `Undo of ${note.reverts ?? ''}`.trim() }) : null,
      note.resent_from ? h('span', { class: 'note-tag', text: `Re-sent from ${note.resent_from}` }) : null,
      iterOn && note.follows ? h('span', { class: 'note-tag', text: `Follow-up on ${note.follows}` }) : null,
      iterOn && isSetAside(note) && note.kind !== 'revert' ? h('span', { class: 'chip tone-muted note-chip' }, h('span', { class: 'chip-text', text: 'Set aside' })) : null));
    li.append(h('p', { class: 'note-text' }, iterOn ? linkedText(note.text || (note.kind === 'revert' ? 'Reverse this change' : ''), note.refs ?? [], onLink) : (note.text || (note.kind === 'revert' ? 'Reverse this change' : ''))));
    const refLine = iterOn ? refLinks(note.text, note.refs, onLink) : null;
    if (refLine) li.append(refLine);
    if (note.read_only) li.append(h('p', { class: 'note-hint', text: earlierText(note, state.session?.id) }));
    if (note.undone_by) li.append(h('p', { class: 'note-hint', text: `Undone by ${note.undone_by}. It no longer counts.` }));
    if (note.no_change) li.append(h('p', { class: 'note-hint', text: 'Done, nothing to change.' }));
    if (note.status === 'done' && note.commit) li.append(h('p', { class: 'note-hint' }, `Commit ${shortSha(note.commit)}`, iterOn && acts.compare ? [' · ', h('button', { class: 'ref-link', type: 'button', onclick: () => openLiveCompare(note) }, 'picture')] : null));
    const fail = failText(note);
    if (note.status === 'failed' && fail) li.append(h('p', { class: 'note-fail', role: 'note', text: fail }));
    // A failed note says why once, in the red box; Archon's own failure reply would repeat it.
    const thread = (note.thread ?? []).filter((t) => !(note.status === 'failed' && fail && t.from === 'archon'));
    if (thread.length) {
      li.append(h('ol', { class: 'note-thread', 'aria-label': `Thread on ${note.id}` }, thread.map((t) => h('li', { class: ['thread-entry', `from-${t.from}`] },
        h('span', { class: 'thread-from', text: { operator: 'You', agent: 'Agent', archon: 'Archon' }[t.from] ?? t.from }), h('span', {}, iterOn ? linkedText(t.text, note.refs ?? [], onLink) : t.text)))));
    }
    const actions = h('div', { class: 'note-actions' });
    if (acts.compare) actions.append(button('Compare', 'compare', () => openLiveCompare(note)));
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

  // ---- chains: a note with its follow-ups, one thread and one follow-up box ----
  const follows = new Map(); // chain id -> {box, open, target}
  function followEntry(chainId) {
    if (!follows.has(chainId)) {
      const entry = { open: false, target: null };
      entry.box = createFollowBox({
        getNotes: () => getState().notes, thumbs,
        onSend: async ({ target, text, refs }) => { const sent = await onFollowUp({ chain: chainId, target, text, refs }); if (sent === false) return false; entry.target = null; entry.open = false; render(); return true; },
      });
      follows.set(chainId, entry);
    }
    return follows.get(chainId);
  }

  function chainCard(chain, state) {
    const live = state.session?.state === 'live';
    const entry = followEntry(chain.id);
    const li = h('li', { class: ['note-chain', selected && chain.versions.some((n) => n.id === selected) && 'is-selected'], id: `chain-${chain.id}` });
    if (chain.count > 1) {
      li.append(h('div', { class: 'chain-head' }, h('strong', { class: 'note-id', text: chain.id }), statusChip(chain.head),
        h('span', { class: 'chip tone-neutral note-chip chain-count' }, h('span', { class: 'chip-text', text: `${chain.count} versions` }))));
    }
    li.append(h('ul', { class: 'chain-versions' }, chain.versions.map((n) => card(n))));
    if (!live || chain.head.read_only) return li;
    const full = versionLimitText(chain);
    const toggle = h('button', { class: 'button button-plain button-small', type: 'button', 'aria-expanded': String(entry.open), disabled: Boolean(full), onclick: () => { entry.open = !entry.open; if (entry.open) entry.target = null; render(); if (entry.open) entry.box.focus(); } },
      icon('plus', { size: 16 }), ' Follow up');
    li.append(h('div', { class: 'note-actions chain-actions' }, toggle, full ? h('span', { class: 'note-hint', text: full }) : null));
    if (entry.open) {
      const target = chain.versions.find((n) => n.id === entry.target) ?? followTarget(chain);
      const newer = chain.versions.filter((n) => n.number > target.number && n.status === 'done' && !isSetAside(n)).map((n) => n.id);
      entry.box.setTarget(target.id, `Follow up on ${target.id}${versionNumber(chain, target) < chain.count ? '' : ' (newest)'}`,
        olderInCode(chain, target) ? `Sending this first undoes ${newer.join(', ')} with new commits, then follows up on ${target.id}.` : '');
      li.append(entry.box.element);
    }
    return li;
  }

  const groupsOf = (notes) => {
    if (!iterations()) return notes.map((n) => ({ key: n.id, note: n }));
    const chains = buildChains(notes);
    const seen = new Set();
    const out = [];
    for (const n of notes) {
      const id = chainOf(n);
      if (id === null) { out.push({ key: n.id, note: n }); continue; }
      if (seen.has(id)) continue;
      seen.add(id);
      out.push({ key: `chain-${id}`, chain: chainById(chains, id) });
    }
    return out;
  };

  function render() {
    const sessionState = getState().session?.state;
    add.hidden = sessionState === 'confirmed' || sessionState === 'ended';
    const state = getState();
    const notes = state.notes;
    const noteSig = (n) => { const acts = noteActions(n, { finishing: state.session?.state === 'finishing', live: state.session?.state === 'live' }); return [n.approved, n.status, n.text, n.thread, n.undone_by, n.fail_reason, n.read_only, n.after_asset, n.final_asset, n.final_included, n.commit, state.session?.state, selected === n.id, acts.drop, acts.undo, n.follows, n.refs, n.set_aside, n.version, n.versions]; };
    const typing = [...follows.values()].find((f) => f.box.element.contains(document.activeElement));
    reconcile(list, groupsOf(notes), {
      key: (g) => g.key,
      signature: (g) => JSON.stringify(g.chain ? [g.chain.versions.map(noteSig), g.chain.count, follows.get(g.chain.id)?.open ?? false, follows.get(g.chain.id)?.target ?? null, state.session?.state] : noteSig(g.note)),
      render: (g) => (g.chain ? chainCard(g.chain, state) : card(g.note)),
    });
    if (typing && !typing.box.element.contains(document.activeElement)) typing.box.focus();
    empty.hidden = notes.length > 0 || pending.children.length > 0;
    title.textContent = notesHeading(notes);
    add.disabled = state.session?.state !== 'live';
    for (const [id, view] of compares) { const latest = notes.find((n) => n.id === id); if (latest) view.update(latest); }
  }

  return {
    element: el, render,
    // Notes typed while offline that have not reached the daemon yet.
    setPending(entries) {
      pending.replaceChildren(...entries.map((e) => h('li', { class: 'note note-unsent' },
        h('div', { class: 'note-head' }, h('span', { class: 'chip tone-amber note-chip' }, icon('clock', { size: 14 }), h('span', { class: 'chip-text', text: 'Waiting to send' }))),
        e.follows ? h('p', { class: 'note-tag', text: `Follow-up on ${e.follows}` }) : null,
        h('p', { class: 'note-text', text: e.text }),
        e.refs?.length ? h('p', { class: 'note-hint note-refs', text: `References: ${e.refs.join(' ')}` }) : null)));
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
    // Open the follow-up box of a note's chain, on that version.
    openFollow(id) {
      const chain = chainOfNote(buildChains(getState().notes), id);
      if (!chain) return false;
      const entry = followEntry(chain.id);
      entry.open = true;
      entry.target = id;
      selected = id;
      render();
      entry.box.focus();
      el.querySelector(`#chain-${CSS.escape(chain.id)}`)?.scrollIntoView?.({ block: 'nearest' });
      return true;
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
