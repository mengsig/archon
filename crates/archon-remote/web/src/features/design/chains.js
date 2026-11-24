// Chains of notes, as pure functions: a note and its follow-ups form a chain,
// and a chain is a tree (a follow-up on an older version branches). Also what
// the follow-up box needs: @D mentions, reference limits and the history line.
import { MAX_REFS, MAX_VERSIONS } from '../../core/design.js';
import { noteLook } from './state.js';

const byNumber = (a, b) => a.number - b.number;
const STOP = new Set(['make', 'change', 'use', 'add', 'set', 'give', 'put', 'turn', 'move', 'remove', 'show', 'hide', 'update', 'fix', 'the', 'a', 'an', 'this', 'that', 'my', 'it', 'to', 'its', 'our', 'your', 'more', 'less', 'please', 'can', 'you', 'be', 'and', 'of', 'in', 'on', 'for', 'green', 'blue', 'red', 'black', 'white', 'gray', 'grey', 'yellow', 'orange', 'purple', 'pink', 'dark', 'light', 'primary', 'secondary', 'big', 'bigger', 'small', 'smaller', 'large', 'larger', 'tall', 'taller', 'wide', 'wider', 'bold', 'round', 'rounder', 'new', 'all', 'some', 'with', 'from', 'like', 'but', 'not']);

// The id of the chain a note belongs to; revert notes belong to none. A daemon
// that sends no chain field gives every note a chain of its own.
export function chainOf(note) {
  if (note.kind === 'revert') return null;
  return note.chain ?? note.id;
}

export function isSetAside(note) {
  return note.set_aside ?? Boolean(note.undone_by);
}

// A short upper-case label from the chain's first words: "Make the header taller" -> "HEADER".
export function chainLabel(note) {
  if (note.kind === 'page') return 'PAGE';
  const word = String(note.text ?? '').toLowerCase().match(/[a-z0-9][a-z0-9-]*/g)?.find((w) => !STOP.has(w));
  return word ? word.toUpperCase() : 'CHANGE';
}

// Chains newest first. Each: {id, label, versions (by number), nodes [{note, depth, parent}] in tree order, count, newest, current, head}.
export function buildChains(notes) {
  const groups = new Map();
  for (const note of notes) {
    const id = chainOf(note);
    if (id === null) continue;
    if (!groups.has(id)) groups.set(id, []);
    groups.get(id).push(note);
  }
  const chains = [];
  for (const [id, list] of groups) {
    const versions = [...list].sort(byNumber);
    const ids = new Set(versions.map((n) => n.id));
    const kids = new Map();
    const roots = [];
    for (const n of versions) {
      if (n.follows && ids.has(n.follows) && n.follows !== n.id) {
        if (!kids.has(n.follows)) kids.set(n.follows, []);
        kids.get(n.follows).push(n);
      } else roots.push(n);
    }
    const nodes = [];
    const seen = new Set();
    const walk = (n, depth, parent) => {
      if (seen.has(n.id)) return;
      seen.add(n.id);
      nodes.push({ note: n, depth, parent });
      for (const k of kids.get(n.id) ?? []) walk(k, depth + 1, n.id);
    };
    for (const r of roots) walk(r, 0, null);
    for (const n of versions) walk(n, 0, null); // a loop in follows never hides a note
    const live = (n) => n.status !== 'cancelled' && n.status !== 'failed' && !isSetAside(n);
    const newest = versions[versions.length - 1];
    const current = [...versions].reverse().find((n) => n.status === 'done' && !isSetAside(n)) ?? null;
    chains.push({
      id, label: chainLabel(versions[0]), versions, nodes, newest, current,
      count: Math.max(versions.length, ...versions.map((n) => n.versions ?? 0)),
      head: [...versions].reverse().find(live) ?? newest,
    });
  }
  return chains.sort((a, b) => b.newest.number - a.newest.number);
}

export function chainById(chains, id) {
  return chains.find((c) => c.id === id) ?? null;
}

export function chainOfNote(chains, noteId) {
  return chains.find((c) => c.versions.some((n) => n.id === noteId)) ?? null;
}

export function versionNumber(chain, note) {
  const at = chain.versions.findIndex((n) => n.id === note.id);
  return note.version > 0 ? note.version : at + 1;
}

export function versionLabel(chain, note) {
  return `${note.id} · version ${versionNumber(chain, note)} of ${chain.count}`;
}

// A done change that is in the code now.
export const inCode = (n) => n.status === 'done' && Boolean(n.commit) && !n.no_change && !isSetAside(n);

// An older version whose chain has a newer version in the code: continuing
// from it reverts those first.
export function olderInCode(chain, note) {
  return inCode(note) && chain.versions.some((n) => n.number > note.number && inCode(n));
}

// The version a follow-up on the chain continues from by default.
export function followTarget(chain) {
  return chain.head;
}

export function versionLimitText(chain) {
  return chain.count >= MAX_VERSIONS ? `This change already has ${MAX_VERSIONS} versions. Start a new note instead.` : '';
}

// The one-line summary of a finish card's earlier versions.
export function historyLine(history, max = 40) {
  if (!Array.isArray(history) || !history.length) return '';
  const cut = (t) => { const s = String(t ?? '').replace(/\s+/g, ' ').trim(); return s.length > max ? `${s.slice(0, max - 1)}…` : s; };
  return `History: ${history.map((h) => `${h.note} ${cut(h.text)}${h.set_aside ? ' (set aside)' : ''}`.trim()).join(' › ')}`;
}

// ---- references ----

// The "@D…" being typed just before the caret, or null.
export function mentionAt(text, caret = text.length) {
  const m = /(^|[\s(])@(D?\d*)$/i.exec(String(text).slice(0, caret));
  return m ? { start: caret - m[2].length - 1, end: caret, query: m[2] } : null;
}

// Earlier notes matching what follows the "@", newest first, not yet chosen.
export function suggestRefs(notes, query, chosen = [], { limit = 6, exclude = [] } = {}) {
  const q = String(query ?? '').toUpperCase().replace(/^D/, '');
  return notes
    .filter((n) => n.kind !== 'revert' && !chosen.includes(n.id) && !exclude.includes(n.id) && (q === '' || String(n.number).startsWith(q)))
    .sort((a, b) => b.number - a.number)
    .slice(0, limit);
}

// The text with the mention replaced by the note id and a space, and the new caret.
export function applyMention(text, mention, id) {
  const head = `${text.slice(0, mention.start)}${id} `;
  return { text: head + text.slice(mention.end), caret: head.length };
}

export function addRef(refs, id) {
  if (refs.includes(id)) return { refs, error: '' };
  if (refs.length >= MAX_REFS) return { refs, error: `A note can refer to at most ${MAX_REFS} earlier notes.` };
  return { refs: [...refs, id], error: '' };
}

// Text cut into plain pieces and the D<n> ids that are real references, which render as links.
export function linkParts(text, refs = []) {
  const out = [];
  let last = 0;
  for (const m of String(text ?? '').matchAll(/\bD\d+\b/g)) {
    if (!refs.includes(m[0])) continue;
    if (m.index > last) out.push({ text: text.slice(last, m.index) });
    out.push({ ref: m[0] });
    last = m.index + m[0].length;
  }
  if (last < text.length) out.push({ text: text.slice(last) });
  return out;
}

// Earlier notes the picker offers: not reverts, not the note's own chain's follow target.
export function pickableRefs(notes, chosen = []) {
  return notes.filter((n) => n.kind !== 'revert' && !chosen.includes(n.id)).sort((a, b) => b.number - a.number);
}
