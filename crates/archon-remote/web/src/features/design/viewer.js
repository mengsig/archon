// The comparison area for a selected version: its before and after pictures
// under a bar that says which version, with Back to current, Compare with…
// and the two ways to continue. The live preview is never touched.
import { h } from '../../ui/dom.js';
import { createCompare } from './compare.js';
import { versionLabel, versionNumber, olderInCode, isSetAside } from './chains.js';
import { shortSha, afterAssetOf } from './state.js';
import { designErrorText } from '../../core/design.js';

export function createViewer({ ctx, onBack, onContinue, onFollowUp }) {
  const bar = h('div', { class: 'viewer-bar', role: 'status' });
  const back = h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => onBack() }, 'Back to current');
  const body = h('div', { class: 'viewer-body' });
  const select = h('select', { class: 'input input-inline viewer-compare-with', 'aria-label': 'Compare with…' });
  const cont = h('button', { class: 'button button-primary button-small', type: 'button' }, 'Continue from this version');
  const follow = h('button', { class: 'button button-plain button-small', type: 'button' }, 'Follow up on this version');
  const el = h('section', { class: 'design-viewer', 'aria-label': 'Viewing a version', hidden: true }, h('div', { class: 'viewer-barrow' }, bar, back), body, h('div', { class: 'viewer-actions' }, select, cont, follow));
  let shownId = null;
  let compare = null;
  let other = '';
  let current = null;

  async function side(chain, a, b) {
    body.replaceChildren(h('div', { class: 'list-status', text: 'Loading…' }));
    const fig = async (n) => {
      const id = afterAssetOf(n);
      const word = `${n.id}·v${versionNumber(chain, n)}`;
      if (!id) return h('figure', { class: 'cmp-pane' }, h('div', { class: 'list-status', text: `${word} has no picture.` }));
      try {
        const asset = await ctx.design.asset(n.item, id);
        return h('figure', { class: 'cmp-pane viewer-pane' }, h('img', { class: 'cmp-img', src: asset.data_url, alt: `After picture of ${n.id}`, width: String(asset.width), height: String(asset.height) }),
          h('figcaption', { class: 'cmp-caption', text: `After ${word}${n.commit ? ` · commit ${shortSha(n.commit)}` : ''}` }));
      } catch (err) { return h('p', { class: 'form-error', role: 'alert', text: designErrorText(err) }); }
    };
    const figs = await Promise.all([fig(a), fig(b)]);
    if (other && current?.note.id === shownId) body.replaceChildren(h('div', { class: 'cmp-stage cmp-pair viewer-side' }, ...figs));
  }

  select.addEventListener('change', () => { other = select.value; if (current) draw(current.chain, current.note); });
  cont.addEventListener('click', () => current && onContinue(current.note));
  follow.addEventListener('click', () => current && onFollowUp(current.note));

  function draw(chain, note) {
    if (other && chain.versions.some((n) => n.id === other)) side(chain, note, chain.versions.find((n) => n.id === other));
    else { other = ''; select.value = ''; body.replaceChildren(compare.content); }
  }

  return {
    element: el,
    get shown() { return shownId; },
    // Show (or refresh) a version of a chain.
    show(chain, note, { act = true } = {}) {
      const fresh = shownId !== note.id;
      current = { chain, note };
      if (fresh) { shownId = note.id; other = ''; compare = createCompare(ctx, note); }
      else compare.update(note);
      const commit = note.commit ? `pictures at commit ${shortSha(note.commit)}` : 'no commit yet';
      bar.replaceChildren(h('strong', { text: `Viewing ${versionLabel(chain, note)}` }), ` (${commit}). The live preview still shows the current version.`);
      select.replaceChildren(h('option', { value: '', text: 'Compare with…' }), ...chain.versions.filter((n) => n.id !== note.id)
        .map((n) => h('option', { value: n.id, text: `${n.id}·v${versionNumber(chain, n)}${chain.current?.id === n.id ? ' (current)' : ''}${isSetAside(n) ? ' (set aside)' : ''}` })));
      select.value = other;
      cont.hidden = !act || !olderInCode(chain, note);
      follow.hidden = !act;
      el.hidden = false;
      if (fresh) draw(chain, note);
    },
    hide() { el.hidden = true; shownId = null; current = null; compare = null; other = ''; body.replaceChildren(); },
  };
}
