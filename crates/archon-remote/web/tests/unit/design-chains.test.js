import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildChains, chainLabel, chainOf, isSetAside, olderInCode, followTarget, historyLine, mentionAt, suggestRefs, applyMention, addRef, linkParts, versionLabel, versionLimitText } from '../../src/features/design/chains.js';
import { nodeLine } from '../../src/features/design/tree.js';
import { hasIterations, MAX_REFS, MAX_VERSIONS } from '../../src/core/design.js';

const note = (number, extra = {}) => ({ id: `D${number}`, number, kind: 'change', text: `note ${number}`, status: 'done', commit: `c${number}abcdef0`, no_change: false, follows: null, refs: [], set_aside: false, ...extra });

// D3 → D7 → D9, and D3's branch D8 off D7's sibling; D5 with D6 set aside and D8 working.
const notes = [
  note(3, { text: 'Make the header taller' }),
  note(5, { text: 'Green primary buttons' }),
  note(6, { follows: 'D5', set_aside: true }),
  note(7, { follows: 'D3', text: 'bigger title' }),
  note(8, { follows: 'D5', status: 'working', commit: null }),
  note(9, { follows: 'D7' }),
  note(10, { kind: 'page', text: 'Combine D3 and D5' }),
  note(11, { kind: 'revert', reverts: 'D9', follows: null }),
];

test('chains group by root, newest first, reverts belong to none', () => {
  const chains = buildChains(notes.map((n) => ({ ...n, chain: n.kind === 'revert' ? null : { D6: 'D5', D7: 'D3', D8: 'D5', D9: 'D3' }[n.id] ?? n.id })));
  assert.deepEqual(chains.map((c) => c.id), ['D10', 'D3', 'D5']);
  assert.equal(chainOf(notes[7]), null);
  const d3 = chains.find((c) => c.id === 'D3');
  assert.deepEqual(d3.nodes.map((n) => [n.note.id, n.depth]), [['D3', 0], ['D7', 1], ['D9', 2]]);
  assert.equal(d3.current.id, 'D9');
});

test('chains fall back to a chain of one when the daemon sends no chain field', () => {
  const chains = buildChains(notes.map(({ follows, ...n }) => n));
  assert.equal(chains.length, 7);
});

test('a follow-up on an older version branches; a set-aside version is not current', () => {
  const chains = buildChains(notes.map((n) => ({ ...n, chain: { D6: 'D5', D7: 'D3', D8: 'D5', D9: 'D3' }[n.id] ?? n.id })));
  const d5 = chains.find((c) => c.id === 'D5');
  assert.deepEqual(d5.nodes.map((n) => [n.note.id, n.depth, n.parent]), [['D5', 0, null], ['D6', 1, 'D5'], ['D8', 1, 'D5']]);
  assert.equal(d5.current.id, 'D5');
  assert.equal(isSetAside(d5.versions[1]), true);
  assert.equal(d5.head.id, 'D8');
  assert.equal(followTarget(d5).id, 'D8');
  assert.equal(d5.count, 3);
});

test('a loop in follows never hides a note', () => {
  const chains = buildChains([note(1, { follows: 'D2', chain: 'D1' }), note(2, { follows: 'D1', chain: 'D1' })]);
  assert.equal(chains[0].nodes.length, 2);
});

test('chain labels come from the first words', () => {
  assert.equal(chainLabel(note(3, { text: 'Make the header taller' })), 'HEADER');
  assert.equal(chainLabel(note(5, { text: 'Green primary buttons' })), 'BUTTONS');
  assert.equal(chainLabel(note(4, { kind: 'page' })), 'PAGE');
  assert.equal(chainLabel(note(4, { text: '' })), 'CHANGE');
});

test('only an older done version with a newer one in the code can be continued from', () => {
  const [chain] = buildChains([note(3, { chain: 'D3' }), note(7, { chain: 'D3', follows: 'D3' }), note(9, { chain: 'D3', follows: 'D7' })]);
  assert.equal(olderInCode(chain, chain.versions[0]), true);
  assert.equal(olderInCode(chain, chain.versions[2]), false);
  const [aside] = buildChains([note(3, { chain: 'D3' }), note(7, { chain: 'D3', follows: 'D3', set_aside: true })]);
  assert.equal(olderInCode(aside, aside.versions[0]), false);
});

test('version labels and the version limit', () => {
  const [chain] = buildChains([note(3, { chain: 'D3' }), note(7, { chain: 'D3', follows: 'D3', version: 2, versions: 2 })]);
  assert.equal(versionLabel(chain, chain.versions[1]), 'D7 · version 2 of 2');
  assert.equal(versionLimitText(chain), '');
  const many = Array.from({ length: MAX_VERSIONS }, (_, i) => note(i + 1, { chain: 'D1', follows: i ? `D${i}` : null }));
  assert.match(versionLimitText(buildChains(many)[0]), /20 versions/);
});

test('the history line joins earlier versions in order', () => {
  assert.equal(historyLine([{ note: 'D3', text: 'Make the header taller', set_aside: false }, { note: 'D7', text: 'bigger title', set_aside: false }]), 'History: D3 Make the header taller › D7 bigger title');
  assert.equal(historyLine([{ note: 'D3', text: 'x', set_aside: true }]), 'History: D3 x (set aside)');
  assert.equal(historyLine([]), '');
  assert.equal(historyLine(undefined), '');
  assert.match(historyLine([{ note: 'D3', text: 'a'.repeat(100) }]), /…$/);
});

test('@D autocomplete finds the mention before the caret', () => {
  assert.deepEqual(mentionAt('Slightly darker, like @D'), { start: 22, end: 24, query: 'D' });
  assert.equal(mentionAt('like @D5 and @')?.query, '');
  assert.equal(mentionAt('mail me@D5'), null);
  assert.equal(mentionAt('no mention'), null);
  assert.equal(mentionAt('@d12 later', 4)?.query, 'd12');
  assert.equal(mentionAt('like @D5 and', 12), null);
});

test('suggestions match by number, skip reverts and chosen ones, newest first', () => {
  assert.deepEqual(suggestRefs(notes, 'D').map((n) => n.id).slice(0, 3), ['D10', 'D9', 'D8']);
  assert.deepEqual(suggestRefs(notes, '1').map((n) => n.id), ['D10']);
  assert.deepEqual(suggestRefs(notes, 'D', ['D10', 'D9']).map((n) => n.id).slice(0, 2), ['D8', 'D7']);
  assert.equal(suggestRefs(notes, 'D', [], { limit: 2 }).length, 2);
  assert.ok(!suggestRefs(notes, 'D').some((n) => n.kind === 'revert'));
});

test('choosing a suggestion replaces the mention with the id', () => {
  const text = 'Slightly darker, like @D and more';
  const m = mentionAt(text, 24);
  assert.deepEqual(applyMention(text, m, 'D5'), { text: 'Slightly darker, like D5  and more', caret: 25 });
});

test('references are limited to five and never repeat', () => {
  let refs = [];
  for (const id of ['D1', 'D2', 'D3', 'D4', 'D5']) refs = addRef(refs, id).refs;
  assert.equal(refs.length, MAX_REFS);
  assert.match(addRef(refs, 'D6').error, /at most 5/);
  assert.equal(addRef(refs, 'D6').refs, refs);
  assert.deepEqual(addRef(refs, 'D3'), { refs, error: '' });
});

test('only ids that are references become links', () => {
  assert.deepEqual(linkParts('Use the green from D5 not D6', ['D5']), [{ text: 'Use the green from ' }, { ref: 'D5' }, { text: ' not D6' }]);
  assert.deepEqual(linkParts('no ids', ['D5']), [{ text: 'no ids' }]);
  assert.deepEqual(linkParts('D5D6 D55', ['D5']), [{ text: 'D5D6 D55' }]);
});

test('tree node lines show follow-ups and short commits', () => {
  assert.equal(nodeLine(note(3, { text: 'Make the header taller', commit: '9ab02f1aaaa' })), 'Make the header taller · 9ab02f1');
  assert.equal(nodeLine(note(7, { follows: 'D3', text: 'bigger title', status: 'working', commit: null })), '↳ follow-up: bigger title');
});

test('the host feature is read from the welcome', () => {
  assert.equal(hasIterations({ features: ['design_sessions', 'design_iterations'] }), true);
  assert.equal(hasIterations({ features: ['design_sessions'] }), false);
  assert.equal(hasIterations(null), false);
});
