import test from 'node:test';
import assert from 'node:assert/strict';
import { createSessionSignal } from '../../src/core/session-signal.js';

// Channels of one name reach each other, never themselves, as BroadcastChannel does.
function fakeChannels() {
  const open = [];
  return class FakeChannel extends EventTarget {
    constructor(name) {
      super();
      this.name = name;
      open.push(this);
    }
    postMessage(data) {
      for (const other of open) {
        if (other !== this && other.name === this.name) other.dispatchEvent(Object.assign(new Event('message'), { data }));
      }
    }
  };
}

test('a pairing or a forget in one tab reaches the other tabs, not itself', () => {
  const Channel = fakeChannels();
  const [mine, other] = [createSessionSignal({ Channel }), createSessionSignal({ Channel })];
  const heard = { mine: [], other: [] };
  mine.listen((reason) => heard.mine.push(reason));
  const unlisten = other.listen((reason) => heard.other.push(reason));
  mine.announce('paired');
  mine.announce('forgotten');
  unlisten();
  mine.announce('paired');
  assert.deepEqual(heard, { mine: [], other: ['paired', 'forgotten'] });
  assert.throws(() => mine.announce('renamed'), /not a session change/);
});

test('without BroadcastChannel a storage write carries the change', () => {
  const written = [];
  const storage = { setItem: (key, value) => written.push([key, value]) };
  const events = new EventTarget();
  const signal = createSessionSignal({ Channel: null, storage, events });
  const heard = [];
  signal.listen((reason) => heard.push(reason));
  signal.announce('paired');
  assert.equal(written.length, 1);
  const [key, value] = written[0];
  events.dispatchEvent(Object.assign(new Event('storage'), { key: 'unrelated', newValue: 'paired 1 2' }));
  events.dispatchEvent(Object.assign(new Event('storage'), { key, newValue: value }));
  assert.deepEqual(heard, ['paired'], 'only the session key counts');
  assert.throws(() => createSessionSignal({ Channel: null, storage: undefined, events }), /channel or storage/);
});
