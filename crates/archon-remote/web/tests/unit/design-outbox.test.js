import test from 'node:test';
import assert from 'node:assert/strict';
import { createNoteOutbox, newClientKey } from '../../src/features/design/outbox.js';

function memory() {
  const m = new Map();
  return { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => m.set(k, v), removeItem: (k) => m.delete(k), has: (k) => m.has(k) };
}
const err = (code) => Object.assign(new Error(code), { code });

test('an offline note is kept and resent with the same client key', async () => {
  const storage = memory();
  const box = createNoteOutbox({ storage, item: 'qi_1', newKey: () => 'web-aa' });
  const entry = box.add({ text: 'hello', asset: null, send_picture: false, details_text: null });
  assert.equal(entry.client_key, 'web-aa');
  const seen = [];
  await box.flush(async (f) => { seen.push(f); throw err('disconnected'); }, 'ds_1');
  assert.equal(box.pending().length, 1, 'kept while offline');
  // a fresh page reads it back from storage
  const again = createNoteOutbox({ storage, item: 'qi_1' });
  await again.flush(async (f) => { seen.push(f); return { id: 'D1' }; }, 'ds_2');
  assert.deepEqual(seen.map((s) => [s.client_key, s.session]), [['web-aa', 'ds_1'], ['web-aa', 'ds_2']]);
  assert.equal(again.pending().length, 0);
  assert.equal(storage.has('archon.design.outbox.qi_1'), false);
});

test('order holds: a failure keeps that note and the ones after it', async () => {
  const box = createNoteOutbox({ storage: memory(), item: 'qi_1', newKey: (() => { let n = 0; return () => `k${n++}`; })() });
  for (const text of ['a', 'b', 'c']) box.add({ text });
  const sent = [];
  await box.flush(async (f) => { if (f.text === 'b') throw err('timeout'); sent.push(f.text); }, 's');
  assert.deepEqual(sent, ['a']);
  assert.deepEqual(box.pending().map((e) => e.text), ['b', 'c']);
});

test('a refusal that cannot change is dropped and reported', async () => {
  const box = createNoteOutbox({ storage: memory(), item: 'qi_1', newKey: () => 'k' });
  box.add({ text: 'x' });
  const refused = [];
  await box.flush(async () => { throw err('gone'); }, 's', { onRefused: (e, e2) => refused.push(e2.code) });
  assert.deepEqual(refused, ['gone']);
  assert.equal(box.pending().length, 0);
});

test('client keys are unique', () => {
  assert.notEqual(newClientKey(() => new Uint8Array([1, 2])), newClientKey(() => new Uint8Array([1, 3])));
});
