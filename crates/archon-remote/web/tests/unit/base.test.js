import test from 'node:test';
import assert from 'node:assert/strict';
import { appBase } from '../../src/core/base.js';

test('the app base is the document directory without its trailing slash', () => {
  assert.equal(appBase('https://box.ts.net/archon/'), '/archon');
  assert.equal(appBase('http://127.0.0.1:7777/'), '');
  assert.throws(() => appBase('https://box.ts.net/archon/pair'), /must end in/);
});
