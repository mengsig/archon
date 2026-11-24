import test from 'node:test';
import assert from 'node:assert/strict';
import { installDom, Element } from '../support/dom-shim.js';

installDom();
Object.defineProperty(Element.prototype, 'style', { get() { return { setProperty() {} }; } });
const { createCompare } = await import('../../src/features/design/compare.js');
const { createViewer } = await import('../../src/features/design/viewer.js');
const { afterAssetOf, hasPictures, noVisibleChange } = await import('../../src/features/design/state.js');

const note = (fields = {}) => ({ id: 'D6', item: 'qi_1', target: { box: {} }, before_asset: 'da_before', base_commit: 'aaa', commit: 'bbb', status: 'done', ...fields });
const ctx = () => { const asked = []; return { asked, design: { asset: async (_item, id) => { asked.push(id); return { data_url: `data:${id}`, width: 1, height: 1, caption: id }; } } }; };
const srcOf = (view, which) => {
  const find = (n) => (n.tag === 'img' ? n : n.children?.map(find).find(Boolean));
  const figure = view.content.children[0].children[which];
  return find(figure)?.attrs.get('src');
};
const settle = () => new Promise((r) => setTimeout(r, 0));

test('the after picture is the note\'s after capture, never the final one while it exists', () => {
  assert.equal(afterAssetOf(note({ after_asset: 'da_after', final_asset: 'da_final', final_included: false })), 'da_after');
  assert.equal(afterAssetOf(note({ final_asset: 'da_final', final_included: false })), null);
  assert.equal(hasPictures(note({ after_asset: 'da_after' })), true);
});

test('a capture already kept when the window opens is shown at once', async () => {
  const c = ctx();
  const view = createCompare(c, note({ after_asset: 'da_after' }));
  await settle();
  assert.equal(srcOf(view, 1), 'data:da_after');
  assert.equal(srcOf(view, 0), 'data:da_before');
});

test('a capture that arrives after the window is open appears without reopening', async () => {
  const c = ctx();
  const view = createCompare(c, note());
  await settle();
  assert.equal(srcOf(view, 1), undefined);
  view.update(note({ after_asset: 'da_after' }));
  await settle();
  assert.equal(srcOf(view, 1), 'data:da_after');
});

test('a note done again shows its newest capture, and only that one', async () => {
  const c = ctx();
  const view = createCompare(c, note({ after_asset: 'da_one' }));
  await settle();
  view.update(note({ after_asset: 'da_two', commit: 'ccc' }));
  view.update(note({ after_asset: 'da_two', commit: 'ccc' }));
  await settle();
  assert.equal(srcOf(view, 1), 'data:da_two');
  assert.deepEqual(c.asked.filter((id) => id !== 'da_before'), ['da_one', 'da_two']);
});

const NOTICE = 'No visible change was captured: Only the hover colour changed.';
const told = (text = NOTICE) => ({ thread: [{ at: 'now', from: 'archon', text }] });

test('a note whose pictures matched has no after picture, whatever Finish took later', () => {
  const unchanged = note({ ...told(), final_asset: 'da_final', final_included: true });
  assert.equal(afterAssetOf(unchanged), null);
  assert.equal(noVisibleChange(unchanged), 'Only the hover colour changed.');
  assert.equal(hasPictures(unchanged), true, 'its before picture and notice can still be opened');
  assert.equal(noVisibleChange(note(told('No visible change was captured'))), 'The agent gave no explanation.');
  assert.equal(noVisibleChange(note({ after_asset: 'da_after' })), null);
  assert.equal(noVisibleChange(note({ thread: [{ at: 'now', from: 'agent', text: NOTICE }] })), null, 'only Archon\'s own notice counts');
  assert.equal(noVisibleChange(note()), null);
  assert.equal(noVisibleChange(note({ after_asset: 'da_after', ...told() })), null, 'a real after picture always wins');
});

test('the window shows the notice, with the before picture and no second one', async () => {
  const c = ctx();
  const view = createCompare(c, note(told()));
  await settle();
  const text = (n) => (n.tag === undefined ? '' : n.children.map((k) => k.data ?? text(k)).join(' '));
  assert.match(text(view.content), /No visible change captured/);
  assert.match(text(view.content), /Only the hover colour changed\./);
  assert.deepEqual(c.asked, ['da_before']);
});

test('a retake that lands while the window is open replaces the notice with the picture', async () => {
  const c = ctx();
  const view = createCompare(c, note(told()));
  await settle();
  view.update(note({ after_asset: 'da_after' }));
  await settle();
  assert.equal(srcOf(view, 1), 'data:da_after');
  assert.equal(srcOf(view, 0), 'data:da_before');
  view.update(note(told()));
  assert.deepEqual(c.asked, ['da_before', 'da_after']);
});

test('the notice and the picture swap whole panes, never a pane in two places', async () => {
  const view = createCompare(ctx(), note(told()));
  await settle();
  const stages = () => view.content.children.map((c) => c.attrs.get('class'));
  assert.deepEqual(stages(), ['cmp-stage cmp-pair']);
  assert.equal(view.content.children[0].children.length, 2, 'the before pane and the notice pane');
  view.update(note({ after_asset: 'da_after' }));
  assert.equal(stages()[0], 'cmp-stage');
  assert.equal(view.content.children[0].children.length, 2);
  assert.equal(view.content.children.length, 2, 'the slider follows its stage');
});

test('a late comparison response cannot replace the latest selected pair', async () => {
  const requested = [];
  let release;
  const asset = (id) => ({ data_url: `data:${id}`, width: 1, height: 1 });
  const ctx = { design: { asset: async (_item, id) => {
    requested.push(id);
    if (id === 'after7') await new Promise((resolve) => { release = resolve; });
    return asset(id);
  } } };
  const versions = [6, 7, 8].map((number) => note({ id: `D${number}`, number, after_asset: `after${number}` }));
  const chain = { versions, count: versions.length, current: versions.at(-1) };
  const viewer = createViewer({ ctx, onBack() {}, onContinue() {}, onFollowUp() {} });
  viewer.show(chain, versions[0]);
  const [picker] = viewer.element.query('select');
  picker.value = 'D7';
  picker.listeners.change();
  picker.value = 'D8';
  picker.listeners.change();
  await settle();
  assert.match(viewer.element.textContent, /After D8·v3/);
  release();
  await settle();
  assert.doesNotMatch(viewer.element.textContent, /After D7·v2/);
  assert.match(viewer.element.textContent, /After D8·v3/);
  const count = requested.length;
  viewer.show(chain, versions[0]);
  assert.equal(requested.length, count, 'an unchanged render reuses the current pair');
});
