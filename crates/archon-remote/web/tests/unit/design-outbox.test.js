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
  const entry = box.add({ session: 'ds_1', text: 'hello', asset: null, send_picture: false, details_text: null });
  assert.equal(entry.client_key, 'web-aa');
  const seen = [];
  await box.flush(async (f) => { seen.push(f); throw err('disconnected'); }, 'ds_1');
  assert.equal(box.pending().length, 1, 'kept while offline');
  // a fresh page reads it back from storage
  const again = createNoteOutbox({ storage, item: 'qi_1' });
  await again.flush(async (f) => { seen.push(f); return { id: 'D1' }; }, 'ds_1');
  assert.deepEqual(seen, [entry, entry]);
  assert.equal(again.pending().length, 0);
  assert.equal(storage.has('archon.design.outbox.qi_1'), false);
});

test('stale and legacy drafts stay readable and do not block the current session', async () => {
  const storage = memory();
  const legacy = { text: 'legacy', client_key: 'old' };
  storage.setItem('archon.design.outbox.qi_1', JSON.stringify([legacy]));
  const box = createNoteOutbox({ storage, item: 'qi_1' });
  const stale = box.add({ session: 'ds_1', client_key: 'first', text: 'earlier session', asset: 'da_old', refs: ['D1'] });
  const current = box.add({ session: 'ds_2', client_key: 'second', text: 'current session' });

  const seen = [];
  await box.flush(async (f) => seen.push(f), undefined);
  assert.deepEqual(seen, []);
  await box.flush(async (f) => seen.push(f), 'ds_2');
  assert.deepEqual(seen, [current]);
  assert.deepEqual(box.pending(), [legacy, stale]);
});

test('a session change during replay stops the rest of the batch', async () => {
  const box = createNoteOutbox({ storage: memory(), item: 'qi_1' });
  const first = box.add({ session: 'ds_1', client_key: 'k1', text: 'first' });
  const second = box.add({ session: 'ds_1', client_key: 'k2', text: 'second' });
  const sent = [];
  let current = 'ds_1';
  await box.flush(async (f) => {
    sent.push(f);
    current = 'ds_2';
  }, 'ds_1', { canSend: () => current === 'ds_1' });
  assert.deepEqual(sent, [first]);
  assert.deepEqual(box.pending(), [second]);
});

test('order holds: a failure keeps that note and the ones after it', async () => {
  const box = createNoteOutbox({ storage: memory(), item: 'qi_1', newKey: (() => { let n = 0; return () => `k${n++}`; })() });
  for (const text of ['a', 'b', 'c']) box.add({ text, session: 's' });
  const sent = [];
  await box.flush(async (f) => { if (f.text === 'b') throw err('timeout'); sent.push(f.text); }, 's');
  assert.deepEqual(sent, ['a']);
  assert.deepEqual(box.pending().map((e) => e.text), ['b', 'c']);
});

test('a gone refusal keeps the draft for recovery and prevents future replay', async () => {
  const box = createNoteOutbox({ storage: memory(), item: 'qi_1', newKey: () => 'k' });
  const entry = box.add({ text: 'x', session: 's' });
  const refused = [];
  await box.flush(async () => { throw err('gone'); }, 's', { onRefused: (e, e2) => refused.push(e2.code) });
  assert.deepEqual(refused, ['gone']);
  assert.deepEqual(box.pending(), [{ ...entry, session_ended: true }]);
  const sent = [];
  await box.flush(async (f) => sent.push(f), 's');
  assert.deepEqual(sent, []);
});

test('a finishing refusal keeps the queue for when the same session returns to editing', async () => {
  const box = createNoteOutbox({ storage: memory(), item: 'qi_1' });
  const first = box.add({ session: 'ds_1', client_key: 'k1', text: 'first' });
  const second = box.add({ session: 'ds_1', client_key: 'k2', text: 'second' });
  const seen = [];
  await box.flush(async (entry) => {
    seen.push(entry);
    throw err('finishing');
  }, 'ds_1');
  assert.deepEqual(seen, [first]);
  assert.deepEqual(box.pending(), [first, second]);
  await box.flush(async (entry) => seen.push(entry), 'ds_2');
  assert.deepEqual(seen, [first]);
  await box.flush(async (entry) => seen.push(entry), 'ds_1');
  assert.deepEqual(seen, [first, first, second]);
  assert.deepEqual(box.pending(), []);
});

test('client keys are unique', () => {
  assert.notEqual(newClientKey(() => new Uint8Array([1, 2])), newClientKey(() => new Uint8Array([1, 3])));
});

test('wire refusals invalid_argument and not_found are dropped, not retried', async () => {
  for (const code of ['invalid_argument', 'not_found', 'stale', 'conflict']) {
    const box = createNoteOutbox({ storage: memory(), item: 'qi_1', newKey: () => 'k' });
    box.add({ text: 'x', session: 's' }); box.add({ text: 'y', client_key: 'k2', session: 's' });
    const refused = [];
    await box.flush(async () => { throw err(code); }, 's', { onRefused: (e, e2) => refused.push(e2.code) });
    assert.deepEqual(refused, [code, code]);
    assert.equal(box.pending().length, 0, `${code} must not block the queue`);
  }
});

test('transient codes keep the note', async () => {
  for (const code of ['disconnected', 'timeout', 'unavailable', 'recovering', 'unauthorized']) {
    const box = createNoteOutbox({ storage: memory(), item: 'qi_1', newKey: () => 'k' });
    box.add({ text: 'x', session: 's' });
    await box.flush(async () => { throw err(code); }, 's');
    assert.equal(box.pending().length, 1, code);
  }
});
