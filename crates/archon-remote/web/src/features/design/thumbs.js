// Picture thumbnails of notes: the after picture, else the before one, else a
// blank box. Assets never change under an id, so each is asked for once.
import { h } from '../../ui/dom.js';
import { afterAssetOf } from './state.js';

export const thumbAsset = (note) => afterAssetOf(note) || note.before_asset || null;

export function createThumbs(ctx, item) {
  const cache = new Map();
  const load = (id) => {
    if (!cache.has(id)) {
      const asking = ctx.design.asset(item, id).catch(() => { cache.delete(id); return null; });
      cache.set(id, asking);
    }
    return cache.get(id);
  };
  return {
    element(note, size = 'm') {
      const box = h('span', { class: ['thumb', `thumb-${size}`], 'aria-hidden': 'true' });
      const id = thumbAsset(note);
      if (id) load(id).then((asset) => { if (asset?.data_url) box.replaceChildren(h('img', { class: 'thumb-img', src: asset.data_url, alt: '' })); });
      return box;
    },
    // A picture by asset id, at a size: 's' | 'm' | 'l'.
    image(id, alt, size = 'l') {
      const box = h('span', { class: ['thumb', `thumb-${size}`] });
      if (id) load(id).then((asset) => { if (asset?.data_url) box.replaceChildren(h('img', { class: 'thumb-img', src: asset.data_url, alt })); });
      return box;
    },
    load,
  };
}
