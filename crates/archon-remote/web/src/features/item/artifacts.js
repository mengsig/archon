// An item's artifact store: the file list, and each file's text in a sheet.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { openSheet } from '../../ui/sheet.js';
import { emptyState, errorNote, spinner } from '../../ui/components.js';
import { bytes } from '../../core/format.js';

export function artifactsSection(ctx, itemId) {
  const box = h('div', { class: 'card list-card' }, spinner('Loading artifacts'));
  const load = async () => {
    try {
      const listing = await ctx.cmds.artifacts(itemId);
      box.replaceChildren(...artifactRows(ctx, listing));
    } catch (err) {
      box.replaceChildren(errorNote(err, load));
    }
  };
  load();
  return box;
}

function artifactRows(ctx, listing) {
  if (!listing.files?.length) return [emptyState({ title: 'No artifacts', text: 'Files the stages publish show up here.' })];
  const rows = listing.files.map((file) => h('button', {
    class: 'list-row', type: 'button', onclick: () => openArtifact(ctx, listing.item, file.name),
  }, icon('file', { size: 20 }), h('span', { class: 'list-row-text', text: file.name }),
  h('span', { class: 'list-row-value', text: bytes(file.bytes) })));
  if (listing.omitted > 0) rows.push(h('p', { class: 'list-foot', text: `${listing.omitted} more not listed` }));
  return rows;
}

function openArtifact(ctx, itemId, name) {
  const text = h('pre', { class: 'artifact-text', tabindex: '0', 'aria-label': `Contents of ${name}` });
  const foot = h('div', { class: 'artifact-foot' }, spinner('Loading file'));
  openSheet({ title: name, content: h('div', { class: 'artifact' }, text, foot), size: 'tall' });
  const loadPage = async (offset) => {
    foot.replaceChildren(spinner('Loading file'));
    try {
      const page = await ctx.cmds.readArtifact(itemId, name, offset);
      text.append(document.createTextNode(page.text));
      foot.replaceChildren(page.next_offset === null || page.next_offset === undefined
        ? h('p', { class: 'list-foot', text: `${bytes(page.total_bytes)} · end of file` })
        : h('button', { class: 'button button-plain', type: 'button', onclick: () => loadPage(page.next_offset) },
          `Load more (${bytes(page.total_bytes - page.next_offset)} left)`));
    } catch (err) {
      foot.replaceChildren(errorNote(err, () => loadPage(offset)));
    }
  };
  loadPage(0);
}
