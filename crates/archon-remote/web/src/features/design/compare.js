// Before / after: a slider (or side by side on wide screens), each picture
// captioned with the commit it came from.
import { h } from '../../ui/dom.js';
import { openSheet } from '../../ui/sheet.js';
import { captionFor, afterAssetOf, noVisibleChange } from './state.js';
import { designErrorText } from '../../core/design.js';

// A note with no target points at nothing on the page: its Before picture is
// the operator's own drawing (a New screen sketch, or a sketch over the frozen
// preview), not a capture of the page. The daemon files such a note as a page
// note with no target and gives the asset view no source field, so this is the
// reliable signal. A drawing is mostly transparent, so it is shown beside the
// After picture rather than under a slider.
export function compareLayout(note) {
  if (!note?.target) return { mode: 'pair', beforeWord: 'Your sketch' };
  return { mode: 'slider', beforeWord: 'Before' };
}

// The after picture is always the one taken after the change for this note:
// its newest after capture. It is read from the note each time, so a capture
// that lands after the window is open, or a note done again, swaps it in.
export function createCompare(ctx, note) {
  const layout = compareLayout(note);
  const pair = layout.mode === 'pair';
  const before = h('figure', { class: 'cmp-pane cmp-before' }, h('div', { class: 'list-status', text: 'Loading…' }));
  const after = h('figure', { class: 'cmp-pane cmp-after' }, h('div', { class: 'list-status', text: 'Loading…' }));
  const split = h('input', { class: 'cmp-range', type: 'range', min: '0', max: '100', value: '50', 'aria-label': 'Comparison position: before on the left, after on the right' });
  const stage = h('div', { class: pair ? 'cmp-stage cmp-pair' : 'cmp-stage' }, before, after);
  const apply = () => stage.style.setProperty('--split', `${split.value}%`);
  split.addEventListener('input', apply);
  apply();
  const controls = pair ? null : h('div', { class: 'cmp-controls' }, h('span', { class: 'cmp-end', text: 'Before' }), split, h('span', { class: 'cmp-end', text: 'After' }));
  const content = h('div', { class: 'cmp' }, stage, controls);
  // The same two panes side by side, the after one holding the notice. A pane
  // lives in one parent at a time, so each mode takes them when it is shown.
  const unchanged = h('div', { class: 'cmp-stage cmp-pair' });
  let showing = 'compare';
  const show = (mode) => {
    if (mode === showing) return;
    showing = mode;
    const host = mode === 'unchanged' ? unchanged : stage;
    host.replaceChildren(before, after);
    content.replaceChildren(...(mode === 'unchanged' ? [unchanged] : [stage, controls]).filter(Boolean));
  };
  const fill = async (figure, id, commit, word, stale = () => false) => {
    try {
      const asset = await ctx.design.asset(note.item, id);
      if (stale()) return;
      // The slider's stage takes the Before picture's proportions; both pictures then scale to fit it.
      if (word === layout.beforeWord && !pair && asset.width && asset.height) stage.style.setProperty('--ar', `${asset.width} / ${asset.height}`);
      figure.replaceChildren(
        h('img', { class: 'cmp-img', src: asset.data_url, alt: `${word} picture of ${note.id}`, width: String(asset.width), height: String(asset.height) }),
        h('figcaption', { class: 'cmp-caption', text: word === 'Your sketch' ? word : `${word}: ${asset.caption ?? captionFor(commit, null)}` }));
    } catch (err) {
      if (!stale()) figure.replaceChildren(h('p', { class: 'form-error', role: 'alert', text: designErrorText(err) }));
    }
  };
  let shown = null;
  const update = (latest) => {
    const why = noVisibleChange(latest);
    if (why !== null) {
      shown = null;
      show('unchanged');
      after.replaceChildren(h('div', { class: 'cmp-unchanged', role: 'status' },
        h('strong', { text: 'No visible change captured' }),
        h('p', { text: `The picture taken after the change matches the one before it. ${why}` })));
      return;
    }
    show('compare');
    const id = afterAssetOf(latest);
    if (!id) { if (!shown) after.replaceChildren(h('div', { class: 'list-status', text: 'The after picture is on its way…' })); return; }
    if (id === shown) return;
    shown = id;
    fill(after, id, latest.captured_commit ?? latest.commit, 'After', () => shown !== id);
  };
  fill(before, note.before_asset, note.base_commit, layout.beforeWord);
  update(note);
  return { content, update };
}

export function openCompare(ctx, note, { onClose = () => {} } = {}) {
  const view = createCompare(ctx, note);
  openSheet({ title: `${note.id}: before and after`, content: view.content, size: 'tall', onClose });
  return view;
}
