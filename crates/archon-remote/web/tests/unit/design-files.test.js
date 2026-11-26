import { test } from 'node:test';
import assert from 'node:assert/strict';
import { webcrypto } from 'node:crypto';
import { CHUNK_SIZE, chunkOffsets, fileLimitMessage, formatSize, transferId } from '../../src/features/design/files.js';
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
