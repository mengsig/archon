// The Tree tab: every note and follow-up as a node, grouped by chain, indented
// by depth so a branch shows. Selecting a node views that version. On a phone
// the node's detail opens inline under it.
import { h } from '../../ui/dom.js';
import { reconcile } from '../../ui/list.js';
import { buildChains, chainOfNote, olderInCode, isSetAside, VERSION_ACTIONS, viewingText } from './chains.js';
import { noteLook, shortSha, afterAssetOf, hasPictures } from './state.js';
import { statusChip } from './notes.js';
import { compareLayout } from './compare.js';

const MAX_INDENT = 6;
const clip = (t, n = 60) => { const s = String(t ?? '').replace(/\s+/g, ' ').trim(); return s.length > n ? `${s.slice(0, n - 1)}…` : s; };

// The line under a node's id: "↳ follow-up: words · commit" or "words · commit".
export function nodeLine(note) {
  const words = clip(note.text || (note.kind === 'page' ? 'Note on the whole page' : ''));
  const commit = note.status === 'done' && note.commit ? ` · ${shortSha(note.commit)}` : '';
  return `${note.follows ? '↳ follow-up: ' : ''}${words}${commit}`;
}

export function createTreePanel({ getState, thumbs, onSelect, getViewing, isPhone, actions, canAct = () => true, onLink = onSelect }) {
  const list = h('div', { class: 'tree-groups' });
  const empty = h('p', { class: 'list-status tree-empty', text: 'No notes yet. Every note and follow-up shows here as a node.' });
  const el = h('section', { class: 'tree', 'aria-label': 'Tree of notes' }, list, empty);

  function node(chain, { note, depth }) {
    const viewing = getViewing() === note.id;
    const aside = isSetAside(note);
    const look = noteLook(note);
    const main = h('button', { class: ['tree-node-main'], type: 'button', 'aria-pressed': String(viewing), 'aria-label': `${note.id}, ${look.label}${aside ? ', set aside' : ''}. View this version`, onclick: () => onSelect(note.id) },
      thumbs.element(note, 'm'),
      h('span', { class: 'tree-node-body' },
        h('span', { class: 'tree-node-top' }, h('strong', { class: 'note-id', text: note.id }), aside ? null : statusChip(note),
          aside ? h('span', { class: 'chip tone-muted note-chip' }, h('span', { class: 'chip-text', text: 'Set aside' })) : null,
          chain.count > 1 && chain.current?.id === note.id ? h('span', { class: 'chip tone-accent note-chip' }, h('span', { class: 'chip-text', text: 'current' })) : null),
        h('span', { class: 'tree-node-line', text: nodeLine(note) })));
    // References sit beside the node's button, as links of their own.
    const refs = note.refs?.length ? h('p', { class: 'tree-node-refs' }, 'like ', ...note.refs.flatMap((id, i) => [i ? ', ' : null,
      h('button', { class: 'ref-link', type: 'button', 'aria-label': `Show ${id} in the tree`, onclick: () => onLink(id) }, id)])) : null;
    const li = h('li', { class: ['tree-node', `depth-${Math.min(depth, MAX_INDENT)}`, aside && 'is-aside', viewing && 'is-viewing'], dataset: { note: note.id } }, main, refs);
    if (viewing && isPhone()) li.append(detail(chain, note));
    return li;
  }

  // Phone: the version's pictures and what can be done with it, inline in the
  // sheet, under the same words the desktop viewer uses.
  function detail(chain, note) {
    const pic = (id, word) => h('figure', { class: 'tree-pic' }, thumbs.image(id, `${word} picture of ${note.id}`, 'l'), h('figcaption', { class: 'cmp-caption', text: word }));
    const button = (label, onclick, primary = false) => h('button', { class: ['button', primary ? 'button-primary' : 'button-plain', 'button-small'], type: 'button', onclick }, label);
    const buttons = h('div', { class: 'note-actions' }, button(VERSION_ACTIONS.back, () => actions.back()));
    if (hasPictures(note)) buttons.append(button(VERSION_ACTIONS.compare, () => actions.compare(note)));
    if (canAct()) buttons.append(button(VERSION_ACTIONS.followUp, () => actions.followUp(note)));
    if (canAct() && olderInCode(chain, note)) buttons.append(button(VERSION_ACTIONS.continueFrom, () => actions.continueFrom(note), true));
    const line = viewingText(chain, note);
    return h('div', { class: 'tree-detail', role: 'group', 'aria-label': `Version ${note.id}` },
      h('p', { class: 'tree-detail-title' }, h('strong', { text: line.title }), line.rest),
      h('div', { class: 'tree-pics' }, pic(note.before_asset, compareLayout(note).beforeWord), pic(afterAssetOf(note), 'After')),
      buttons);
  }

  function render() {
    const chains = buildChains(getState().notes);
    const viewing = getViewing();
    const phone = isPhone();
    reconcile(list, chains, {
      key: (c) => c.id,
      signature: (c) => JSON.stringify([canAct(), c.nodes.map(({ note: n, depth }) => [n.id, depth, n.status, n.text, n.commit, n.before_asset, n.after_asset, n.set_aside, n.undone_by, n.follows, n.refs]), c.count, c.current?.id, viewing && c.versions.some((n) => n.id === viewing) ? viewing : null, phone]),
      render: (c) => h('section', { class: 'tree-group', 'aria-label': `${c.id} ${c.label}` },
        h('h3', { class: 'tree-group-title', text: `${c.id} · ${c.label}` }),
        h('ul', { class: 'tree-nodes' }, c.nodes.map((n) => node(c, n)))),
    });
    empty.hidden = chains.length > 0;
  }
  return { element: el, render, chainOf: (id) => chainOfNote(buildChains(getState().notes), id) };
}
