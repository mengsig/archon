// The comparison area for a selected version: its before and after pictures
// under a bar that says which version, with Back to current, Compare with…
// and the two ways to continue. The live preview is never touched.
import { h } from '../../ui/dom.js';
import { createCompare } from './compare.js';
import { versionNumber, olderInCode, isSetAside, VERSION_ACTIONS, viewingText } from './chains.js';
import { shortSha, afterAssetOf } from './state.js';
import { designErrorText } from '../../core/design.js';

export function createViewer({ ctx, onBack, onContinue, onFollowUp }) {
  const bar = h('div', { class: 'viewer-bar', role: 'status' });
  const back = h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => onBack() }, VERSION_ACTIONS.back);
  const body = h('div', { class: 'viewer-body' });
  const select = h('select', { class: 'input input-inline viewer-compare-with', 'aria-label': VERSION_ACTIONS.compareWith });
  const cont = h('button', { class: 'button button-primary button-small', type: 'button' }, VERSION_ACTIONS.continueFrom);
  const follow = h('button', { class: 'button button-plain button-small', type: 'button' }, VERSION_ACTIONS.followUp);
  const el = h('section', { class: 'design-viewer', 'aria-label': 'Viewing a version', hidden: true }, h('div', { class: 'viewer-barrow' }, bar, back), body, h('div', { class: 'viewer-actions' }, select, cont, follow));
  let shownId = null;
  let compare = null;
  let other = '';
  let current = null;
  let loadingPair = null;
  let optionsKey = '';
  let barText = '';

  async function side(chain, a, b) {
    const key = JSON.stringify([a, b].map((n) => [n.id, versionNumber(chain, n), afterAssetOf(n), n.commit]));
    if (loadingPair?.key === key) return;
    const request = { key };
    loadingPair = request;
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
    if (loadingPair === request) body.replaceChildren(h('div', { class: 'cmp-stage cmp-pair viewer-side' }, ...figs));
  }

  select.addEventListener('change', () => { other = select.value; if (current) draw(current.chain, current.note); });
  cont.addEventListener('click', () => current && onContinue(current.note));
  follow.addEventListener('click', () => current && onFollowUp(current.note));

  function draw(chain, note) {
    const selected = other ? chain.versions.find((n) => n.id === other) : null;
    if (selected) side(chain, note, selected);
    else {
      loadingPair = null;
      other = '';
      select.value = '';
      if (body.firstChild !== compare.content) body.replaceChildren(compare.content);
    }
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
      const line = viewingText(chain, note);
      const nextBar = line.title + line.rest;
      if (barText !== nextBar) {
        barText = nextBar;
        bar.replaceChildren(h('strong', { text: line.title }), document.createTextNode(line.rest));
      }
      const options = chain.versions.filter((n) => n.id !== note.id).map((n) => [
        n.id, `${n.id}·v${versionNumber(chain, n)}${chain.current?.id === n.id ? ' (current)' : ''}${isSetAside(n) ? ' (set aside)' : ''}`,
      ]);
      const nextKey = JSON.stringify(options);
      if (optionsKey !== nextKey) {
        optionsKey = nextKey;
        select.replaceChildren(h('option', { value: '', text: VERSION_ACTIONS.compareWith }),
          ...options.map(([value, text]) => h('option', { value, text })));
      }
      if (select.value !== other) select.value = other;
      cont.hidden = !act || !olderInCode(chain, note);
      follow.hidden = !act;
      el.hidden = false;
      draw(chain, note);
    },
    hide() {
      if (el.hidden) return;
      el.hidden = true;
      shownId = null;
      current = null;
      compare = null;
      other = '';
      loadingPair = null;
      body.replaceChildren();
    },
  };
}
