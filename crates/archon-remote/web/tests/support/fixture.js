// The flows' fixture against the dev bridge: a real archond, seeded, and
// the dev server in front of it on a free port.
import assert from 'node:assert/strict';
import { startArchond } from './archond.js';
import { seedDemo } from './seed.js';
import { startDevServer, DEV_CODE } from '../../dev/server.js';

export async function startDevFixture({ archonBin, seed = true } = {}) {
  const archond = await startArchond({ archonBin });
  let dev = null;
  try {
    const seeded = seed ? await seedDemo(archond) : null;
    dev = await startDevServer({ socket: archond.socket, requirePairing: true });
    assert.equal(dev.protocolVersion, archond.protocolVersion, 'the bridge and the helper agree on the protocol');
    return {
      baseURL: dev.url,
      archond,
      seeded,
      dev,
      pairCode: async () => DEV_CODE,
      stop: async () => {
        await dev.close();
        await archond.stop();
      },
    };
  } catch (err) {
    if (dev) await dev.close();
    await archond.stop();
    throw err;
  }
}
