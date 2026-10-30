import test from 'node:test';
import assert from 'node:assert/strict';
import { normalizePairingCode, pairingCodeProblem, formatPairingCode, codeFromHash } from '../../src/core/pairing-code.js';
import { suggestDeviceName, deviceNameProblem } from '../../src/core/device-name.js';

test('codes ignore case, spaces and dashes', () => {
  assert.equal(normalizePairingCode(' abcd-efgh '), 'ABCDEFGH');
  assert.equal(normalizePairingCode('ab cd\tef-gh'), 'ABCDEFGH');
});

test('Crockford look-alikes read as digits', () => {
  assert.equal(normalizePairingCode('DEVCODE1'), 'DEVC0DE1');
  assert.equal(normalizePairingCode('il0o'), '1100');
});

test('only complete Crockford codes can be sent', () => {
  assert.equal(pairingCodeProblem('ABCD-EFGH'), null);
  assert.match(pairingCodeProblem('ABCD'), /all 8/);
  assert.match(pairingCodeProblem('ABCDEFGHJ'), /8 characters/);
  assert.match(pairingCodeProblem('ABCD-EFGU'), /only the letters/);
  assert.match(pairingCodeProblem('ABCD!EFG'), /only the letters/);
});

test('codes are shown as two groups of four', () => {
  assert.equal(formatPairingCode('abcdefgh'), 'ABCD-EFGH');
  assert.equal(formatPairingCode('abc'), 'ABC');
  assert.equal(formatPairingCode('abcde'), 'ABCD-E');
  assert.equal(formatPairingCode('abcdefghjk'), 'ABCD-EFGH');
});

test('a pairing link carries its code in the fragment', () => {
  assert.equal(codeFromHash('#code=abcd-efgh'), 'ABCDEFGH');
  assert.equal(codeFromHash('#/queue'), null);
  assert.equal(codeFromHash(''), null);
});

test('device names come from the user agent and stay within 64 characters', () => {
  assert.equal(suggestDeviceName('Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X)'), 'iPhone');
  assert.equal(suggestDeviceName('Mozilla/5.0 (Linux; Android 15; Pixel 9) Mobile Safari'), 'Android phone');
  assert.equal(suggestDeviceName('Mozilla/5.0 (Macintosh; Intel Mac OS X 15_0)'), 'Mac');
  assert.equal(suggestDeviceName(''), 'Browser');
  assert.equal(deviceNameProblem('  '), 'Give this device a name.');
  assert.equal(deviceNameProblem('x'.repeat(64)), null);
  assert.match(deviceNameProblem('x'.repeat(65)), /64/);
});
