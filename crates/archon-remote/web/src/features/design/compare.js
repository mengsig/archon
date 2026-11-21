// Before / after: a slider (or side by side on wide screens), each picture
// captioned with the commit it came from.
import { h } from '../../ui/dom.js';
import { openSheet } from '../../ui/sheet.js';
import { captionFor } from './state.js';
import { designErrorText } from '../../core/design.js';

export function openCompare(ctx, note) {
  const before = h('figure', { class: 'cmp-pane cmp-before' }, h('div', { class: 'list-status', text: 'Loading…' }));
  const after = h('figure', { class: 'cmp-pane cmp-after' }, h('div', { class: 'list-status', text: 'Loading…' }));
  const split = h('input', { class: 'cmp-range', type: 'range', min: '0', max: '100', value: '50', 'aria-label': 'Comparison position: before on the left, after on the right' });
  const stage = h('div', { class: 'cmp-stage' }, before, after);
  const apply = () => stage.style.setProperty('--split', `${split.value}%`);
  split.addEventListener('input', apply);
  apply();
  const content = h('div', { class: 'cmp' }, stage, h('div', { class: 'cmp-controls' }, h('span', { class: 'cmp-end', text: 'Before' }), split, h('span', { class: 'cmp-end', text: 'After' })));
  openSheet({ title: `${note.id}: before and after`, content, size: 'tall' });
  const fill = async (figure, id, commit, word) => {
    try {
      const asset = await ctx.design.asset(note.item, id);
      // The slider's stage takes the Before picture's proportions; both pictures then scale to fit it.
      if (word === 'Before' && asset.width && asset.height) stage.style.setProperty('--ar', `${asset.width} / ${asset.height}`);
      figure.replaceChildren(
        h('img', { class: 'cmp-img', src: asset.data_url, alt: `${word} picture of ${note.id}`, width: String(asset.width), height: String(asset.height) }),
        h('figcaption', { class: 'cmp-caption', text: `${word}: ${asset.caption ?? captionFor(commit, null)}` }));
    } catch (err) {
      figure.replaceChildren(h('p', { class: 'form-error', role: 'alert', text: designErrorText(err) }));
    }
  };
  fill(before, note.before_asset, note.base_commit, 'Before');
  fill(after, note.after_asset, note.commit, 'After');
}
