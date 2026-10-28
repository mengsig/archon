// The flows' fixture against archond's own remote listener: a real archond,
// seeded, set up and paired with `archon remote` as an operator would.
import assert from 'node:assert/strict';
import { startArchond } from './archond.js';
import { seedDemo } from './seed.js';

const LISTENER_CONFIG = '[remote]\naddress = "127.0.0.1:0"\n';

async function remoteJson(archond, args) {
  const { stdout } = await archond.run(['--json', 'remote', ...args]);
  return JSON.parse(stdout);
}

export async function startListenerFixture({ archonBin, seed = true } = {}) {
  const archond = await startArchond({ archonBin, config: LISTENER_CONFIG });
  try {
    const seeded = seed ? await seedDemo(archond) : null;
    const setup = await remoteJson(archond, ['setup', '--local', '--no-pair']);
    assert.equal(setup.remote.enabled, true, `remote setup left it off: ${JSON.stringify(setup)}`);
    assert.match(setup.remote.listening ?? '', /^127\.0\.0\.1:\d+$/, 'the listener stays on loopback');
    return {
      baseURL: `http://${setup.remote.listening}`,
      archond,
      seeded,
      pairCode: async () => {
        const pairing = await remoteJson(archond, ['pair']);
        assert.match(pairing.code, /^[0-9A-Z]{4}-[0-9A-Z]{4}$/, 'archond opened a pairing code');
        return pairing.code;
      },
      devices: async () => (await remoteJson(archond, ['devices'])).devices,
      remote: (args) => remoteJson(archond, args),
      stop: () => archond.stop(),
    };
  } catch (err) {
    await archond.stop();
    throw err;
  }
}
