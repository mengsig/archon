import test from 'node:test';
import assert from 'node:assert/strict';
import { translate, ALLOWED_COMMANDS } from '../../dev/bridge.js';

test('hello goes on as the CLI client, other allowed frames unchanged', () => {
  const hello = translate(JSON.stringify({ req: 1, cmd: 'hello', protocol_version: 23, client: 'archon-web', client_version: 'web' }));
  assert.deepEqual(hello.forward, { req: 1, cmd: 'hello', protocol_version: 23, client: 'archon-cli', client_version: 'web' });
  const say = translate(JSON.stringify({ req: 4, cmd: 'say', text: 'hi' }));
  assert.deepEqual(say.forward, { req: 4, cmd: 'say', text: 'hi' });
});

test('commands outside the allowlist are refused with forbidden', () => {
  const out = translate(JSON.stringify({ req: 7, cmd: 'ask_operator', text: 'x' }));
  assert.equal(out.forward, undefined);
  assert.equal(out.refuse.id, 7);
  assert.equal(out.refuse.result.Err.code, 'forbidden');
  assert.ok(ALLOWED_COMMANDS.has('answer') && !ALLOWED_COMMANDS.has('shutdown'));
});

test('malformed frames close the socket', () => {
  assert.ok(translate('not json').close);
  assert.ok(translate('[1]').close);
  assert.ok(translate(JSON.stringify({ cmd: 'status' })).close);
  assert.ok(translate(Buffer.from('{}'), true).close);
});
