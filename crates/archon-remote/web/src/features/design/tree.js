// The Tree tab: every note and follow-up as a node, grouped by chain, indented
// by depth so a branch shows. Selecting a node views that version. On a phone
// the node's detail opens inline under it.
import { h } from '../../ui/dom.js';
import { reconcile } from '../../ui/list.js';
import { buildChains, chainOfNote, versionLabel, olderInCode, isSetAside } from './chains.js';
import { noteLook, shortSha, afterAssetOf, hasPictures } from './state.js';
import { statusChip } from './notes.js';

const MAX_INDENT = 6;
const clip = (t, n = 60) => { const s = String(t ?? '').replace(/\s+/g, ' ').trim(); return s.length > n ? `${s.slice(0, n - 1)}…` : s; };

// The line under a node's id: "↳ follow-up: words · commit" or "words · commit".
export function nodeLine(note) {
  const words = clip(note.text || (note.kind === 'page' ? 'Note on the whole page' : ''));
  const commit = note.status === 'done' && note.commit ? ` · ${shortSha(note.commit)}` : '';
  return `${note.follows ? '↳ follow-up: ' : ''}${words}${commit}`;
}

export function createTreePanel({ getState, thumbs, onSelect, getViewing, isPhone, actions, canAct = () => true }) {
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
    const li = h('li', { class: ['tree-node', `depth-${Math.min(depth, MAX_INDENT)}`, aside && 'is-aside', viewing && 'is-viewing'], dataset: { note: note.id } }, main);
    if (viewing && isPhone()) li.append(detail(chain, note));
    return li;
  }

  // Phone: the version's pictures and what can be done with it, inline in the sheet.
  function detail(chain, note) {
    const pic = (id, word) => h('figure', { class: 'tree-pic' }, thumbs.image(id, `${word} picture of ${note.id}`, 'l'), h('figcaption', { class: 'cmp-caption', text: word }));
    const buttons = h('div', { class: 'note-actions' });
    if (hasPictures(note)) buttons.append(h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => actions.compare(note) }, 'Open compare'));
    if (canAct()) buttons.append(h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => actions.followUp(note) }, 'Follow up'));
    if (canAct() && olderInCode(chain, note)) buttons.append(h('button', { class: 'button button-primary button-small', type: 'button', onclick: () => actions.continueFrom(note) }, 'Continue from here'));
    return h('div', { class: 'tree-detail', role: 'group', 'aria-label': `Version ${note.id}` },
      h('p', { class: 'tree-detail-title' }, h('strong', { text: versionLabel(chain, note) }), note.commit ? ` ${shortSha(note.commit)}` : ''),
      h('div', { class: 'tree-pics' }, pic(note.before_asset, 'Before'), pic(afterAssetOf(note), 'After')),
      buttons);
  }

  function render() {
    const chains = buildChains(getState().notes);
    const viewing = getViewing();
    const phone = isPhone();
    reconcile(list, chains, {
      key: (c) => c.id,
      signature: (c) => JSON.stringify([canAct(), c.nodes.map(({ note: n, depth }) => [n.id, depth, n.status, n.text, n.commit, n.before_asset, n.after_asset, n.set_aside, n.undone_by, n.follows]), c.count, c.current?.id, viewing && c.versions.some((n) => n.id === viewing) ? viewing : null, phone]),
      render: (c) => h('section', { class: 'tree-group', 'aria-label': `${c.id} ${c.label}` },
        h('h3', { class: 'tree-group-title', text: `${c.id} · ${c.label}` }),
        h('ul', { class: 'tree-nodes' }, c.nodes.map((n) => node(c, n)))),
    });
    empty.hidden = chains.length > 0;
  }
  return { element: el, render, chainOf: (id) => chainOfNote(buildChains(getState().notes), id) };
}
