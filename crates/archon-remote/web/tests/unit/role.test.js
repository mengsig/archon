import test from 'node:test';
import assert from 'node:assert/strict';
import { installDom, html } from '../support/dom-shim.js';
import { canOperate, assertOperator } from '../../src/core/role.js';
import { parseRoute } from '../../src/core/router.js';

installDom();
const { questionCard } = await import('../../src/features/inbox/question-card.js');
const { itemActions } = await import('../../src/features/item/actions.js');

const question = { id: 'q1', number: 3, text: 'Ship it?', options: ['Option A', 'Option B'], asked_at: new Date().toISOString(), item: null };

test('only an operator device operates; an unknown or missing role fails closed', () => {
  assert.equal(canOperate({ id: 'dev_1', role: 'operator' }), true);
  assert.equal(canOperate({ id: 'dev_1', role: 'observer' }), false);
  assert.equal(canOperate({ id: 'dev_1', role: 'admin' }), false);
  assert.equal(canOperate({ id: 'dev_1' }), false);
  assert.throws(() => canOperate(null), /names its device/);
  assert.throws(() => canOperate({ id: 'dev_1', role: 1 }), /role is a string/);
});

test('an operator-only screen refuses to open on an observer device', () => {
  assert.doesNotThrow(() => assertOperator({ canOperate: true }, 'Commands'));
  assert.throws(() => assertOperator({ canOperate: false }, 'Commands'), /Commands is an operator action/);
  assert.throws(() => assertOperator({}, 'New item'), /needs the app context/);
});

test('an observer sees a question and its options with no way to answer', () => {
  const card = html(questionCard({ canOperate: false }, question, null));
  assert.match(card, /Ship it\?/);
  assert.match(card, /Options: Option A · Option B/);
  assert.doesNotMatch(card, /<button|<form|<input/);
});

test('an operator answers a question by option or free text', () => {
  const card = html(questionCard({ canOperate: true }, question, null));
  assert.match(card, /<button class="option-button" type="button">Option A<\/button>/);
  assert.match(card, /aria-label="Send answer"/);
  assert.match(card, /aria-label="Answer Q3"/);
});

test('an observer gets no item actions; an operator gets them all', () => {
  const detail = { item: { id: 'qi_1', title: 'T', repo: 'repo_1', pipeline: 'build', state: { state: 'succeeded' } }, merges: [] };
  const store = { get: () => ({ status: { pause: { global: false, repos: [], pipelines: [] } } }) };
  assert.equal(itemActions({ canOperate: false, store }, detail, { reload: () => {}, repoName: 'r' }), null);
  const bar = html(itemActions({ canOperate: true, store }, detail, { reload: () => {}, repoName: 'r' }));
  for (const label of ['Commands', 'Merge', 'Pause repo r', 'Pause pipeline build']) assert.match(bar, new RegExp(`aria-label="${label}"`));
  assert.throws(() => itemActions({ store }, detail, { reload: () => {} }), /device role/);
});

test('settings is its own route, so no tab claims it', () => {
  assert.equal(parseRoute('#/settings').tab, 'settings');
  assert.equal(parseRoute('#/settings').name, 'settings');
});
