// The flows' fixture against archond's own remote listener: a real archond,
// seeded, set up and paired with `archon remote` as an operator would.
import assert from 'node:assert/strict';
import { startArchond } from './archond.js';
import { seedDemo } from './seed.js';
import { startMount, PREFIX } from './mount.js';

const LISTENER_CONFIG = '[remote]\naddress = "127.0.0.1:0"\n';

async function remoteJson(archond, args) {
  const { stdout } = await archond.run(['--json', 'remote', ...args]);
  return JSON.parse(stdout);
}

// `mounted` puts a stand-in for Aether's `/archon` mount in front of archond,
// and the fixture's baseURL is then the app's address behind it.
export async function startListenerFixture({ archonBin, seed = true, mounted = false } = {}) {
  const mount = mounted ? await startMount() : null;
  const config = mount
    ? `${LISTENER_CONFIG}mounted_at = "${PREFIX}"\ntrusted_origins = ["${mount.origin}"]\n`
    : LISTENER_CONFIG;
  const archond = await startArchond({ archonBin, config });
  try {
    const seeded = seed ? await seedDemo(archond) : null;
    const remote = await remoteJson(archond, ['enable']);
    assert.equal(remote.enabled, true, `remote enable left it off: ${JSON.stringify(remote)}`);
    assert.match(remote.listening ?? '', /^127\.0\.0\.1:\d+$/, 'the listener stays on loopback');
    const port = Number(remote.listening.split(':')[1]);
    mount?.forwardTo(port);
    return {
      baseURL: mount ? `${mount.origin}${PREFIX}` : `http://${remote.listening}`,
      listeningPort: port,
      archond,
      seeded,
      pairCode: async () => {
        const pairing = await remoteJson(archond, ['pair']);
        assert.match(pairing.code, /^[0-9A-Z]{4}-[0-9A-Z]{4}$/, 'archond opened a pairing code');
        return pairing.code;
      },
      devices: async () => (await remoteJson(archond, ['devices'])).devices,
      remote: (args) => remoteJson(archond, args),
      stop: async () => { await archond.stop(); await mount?.stop(); },
    };
  } catch (err) {
    await archond.stop();
    await mount?.stop();
    throw err;
  }
}
