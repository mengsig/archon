import { h } from '../../ui/dom.js';
import { section, card, row, chip } from '../../ui/components.js';
import { safeHttpUrl, dateTime } from '../../core/format.js';

export function deliverySection(detail) {
  if (!Array.isArray(detail.merges ?? [])) throw new TypeError('Item merge history must be an array');
  if (!detail.merges?.length) return null;
  return section('Delivery', card(...detail.merges.map((merge) => {
    const url = safeHttpUrl(merge.pr_url);
    const state = typeof merge.state === 'string' ? merge.state : merge.state?.state;
    if (!state) throw new TypeError('A merge needs its recorded state');
    const tone = state === 'merged' ? 'green' : ['declined', 'refused'].includes(state) ? 'red' : 'amber';
    return h('div', { class: 'merge-record' }, row('Merge', chip({ label: state.replaceAll('_', ' '), tone })),
      row('Updated', dateTime(merge.updated_at)),
      merge.checks ? row('Checks', merge.checks) : null,
      merge.refusal ? row('Reason', merge.refusal) : null,
      url ? row('Pull request', h('a', { class: 'link-button', href: url, target: '_blank', rel: 'noopener noreferrer' }, 'Open pull request ↗'))
        : row('Local branch', merge.pr_url));
  })));
}
