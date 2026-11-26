import { test } from 'node:test';
import assert from 'node:assert/strict';
import { webcrypto } from 'node:crypto';
import { CHUNK_SIZE, chunkOffsets, acceptMessage, fileLimitMessage, formatSize, transferId } from '../../src/features/design/files.js';
import { initialState, reduce } from '../../src/features/design/state.js';

test('file limits and chunk boundaries', () => {
  assert.deepEqual(chunkOffsets(0), [0]);
  assert.deepEqual(chunkOffsets(CHUNK_SIZE + 1), [0, CHUNK_SIZE]);
  assert.throws(() => chunkOffsets(-1), RangeError);
  assert.equal(fileLimitMessage([{ size: 64 * 1024 * 1024 }]), null);
  assert.match(fileLimitMessage([{ size: 64 * 1024 * 1024 + 1 }]), /64 MiB/);
  assert.match(fileLimitMessage(Array.from({ length: 33 }, () => ({ size: 0 }))), /32/);
  assert.match(fileLimitMessage(Array.from({ length: 5 }, () => ({ size: 64 * 1024 * 1024 }))), /256 MiB/);
});

test('transfer ids and readable sizes', () => {
  globalThis.crypto ??= webcrypto;
  assert.match(transferId(), /^[A-Za-z0-9_-]{1,40}$/);
  assert.notEqual(transferId(), transferId());
  assert.equal(formatSize(0), '0 B');
  assert.equal(formatSize(1536), '1.5 KiB');
  assert.throws(() => formatSize(-1), RangeError);
});

test('browser events are retained until consumed', () => {
  let state = initialState('item');
  for (const event of ['file_chooser', 'download', 'picker', 'clipboard']) state = reduce(state, { type: 'event', event: { event, id: 1 } });
  assert.deepEqual(state.browserEvents.map((event) => event.event), ['file_chooser', 'download', 'picker', 'clipboard']);
  assert.deepEqual(reduce(state, { type: 'browser_events_taken' }).browserEvents, []);
});

test('the accept attribute is enforced on names and types', () => {
  const file = (name, type = '') => ({ name, type });
  assert.equal(acceptMessage('', [file('evil.exe')]), null);
  assert.equal(acceptMessage('.txt,.csv', [file('PLAN.CSV'), file('a.txt')]), null);
  assert.equal(acceptMessage('.txt, .csv', [file('evil.exe')]), 'That file type is not one the app accepts here (.txt, .csv).');
  assert.match(acceptMessage('.txt', [file('a.txt'), file('b.exe')]), /not one the app accepts/);
  assert.match(acceptMessage('.txt', [file('.txt')]), /not one/);
  assert.equal(acceptMessage('text/csv', [file('x', 'text/csv')]), null);
  assert.match(acceptMessage('text/csv', [file('x.txt', 'text/plain')]), /not one/);
  assert.equal(acceptMessage('image/*', [file('p.png', 'image/png')]), null);
  assert.match(acceptMessage('image/*', [file('a.mp3', 'audio/mpeg')]), /not one/);
});

test('a type from the extension stands in for an empty or mismatched File.type', () => {
  const file = (name, type = '') => ({ name, type });
  assert.equal(acceptMessage('image/*', [file('logo.PNG')]), null);
  assert.equal(acceptMessage('image/png', [file('logo.png', '')]), null);
  assert.equal(acceptMessage('text/csv', [file('plan.csv', 'application/vnd.ms-excel')]), null);
  assert.equal(acceptMessage('application/vnd.ms-excel', [file('plan.csv', 'application/vnd.ms-excel')]), null);
  assert.equal(acceptMessage('image/*', [file('scan.raw', 'image/x-raw; charset=x')]), null);
});

test('a file both the client and the daemon refuse stays refused', () => {
  const file = (name, type = '') => ({ name, type });
  assert.match(acceptMessage('image/*', [file('song.mp3')]), /not one/);
  assert.match(acceptMessage('image/png', [file('unknown.xyz')]), /not one/);
  assert.match(acceptMessage('image/png', [file('noext', 'bogus')]), /not one/);
  assert.match(acceptMessage('text/csv', [file('.csv')]), /not one/);
  assert.match(acceptMessage('.k', [file('x.\u212a')]), /not one/);
  assert.match(acceptMessage('.\u212a', [file('x.k')]), /not one/);
  assert.match(acceptMessage('*/*', [file('a.png', 'image/png')]), /not one/);
});

test('the client matches the name the daemon sanitizes and trims like Rust', () => {
  const file = (name, type = '') => ({ name, type });
  assert.equal(acceptMessage('.csv', [file('report.csv ')]), null);
  assert.equal(acceptMessage('text/csv', [file('report.csv\u00a0')]), null);
  assert.equal(acceptMessage('.csv', [file('..report.csv')]), null);
  assert.equal(acceptMessage('.csv', [file('C:\\tmp\\a.csv')]), null);
  const cutInTheExtension = `${'a'.repeat(118)}.csv`;
  assert.match(acceptMessage('.csv', [file(cutInTheExtension)]), /not one/);
  assert.equal(acceptMessage('.csv', [file(`${'a'.repeat(116)}.csv`)]), null);
  assert.equal(acceptMessage('.csv', [file(`${'é'.repeat(58)}.csv`)]), null);
  assert.match(acceptMessage('.csv', [file(`${'é'.repeat(59)}.csv`)]), /not one/);
  assert.match(acceptMessage('.csv', [file('a.csv\ufeff')]), /not one/);
});

test('a byte order mark is not whitespace, as in Rust', () => {
  const file = (name, type = '') => ({ name, type });
  assert.match(acceptMessage('\ufeff.csv', [file('a.csv')]), /not one/);
  assert.equal(acceptMessage('\u00a0.csv', [file('a.csv')]), null);
  assert.match(acceptMessage('text/csv', [file('x', '\ufefftext/csv')]), /not one/);
});

test('a new tab note stays until the operator goes back or reloads', () => {
  let state = initialState('item');
  state = reduce(state, { type: 'event', event: { event: 'new_tab', message: 'The app opened a new tab.' } });
  assert.equal(state.newTab, 'The app opened a new tab.');
  assert.deepEqual(state.toasts, ['The app opened a new tab.']);
  state = reduce(state, { type: 'toasts_taken' });
  assert.equal(state.newTab, 'The app opened a new tab.');
  assert.equal(reduce(state, { type: 'new_tab_dismissed' }).newTab, null);
  assert.equal(reduce(state, { type: 'event', event: { event: 'ended', reason: 'done' } }).newTab, null);
});

test('a refused download reaches the operator as a toast naming the file and the limit', () => {
  const message = "The app's download of stowage.bin was not kept: it is larger than 64 MiB.";
  const state = reduce(initialState('item'), { type: 'event', event: { event: 'toast', message } });
  assert.deepEqual(state.toasts, [message]);
});
