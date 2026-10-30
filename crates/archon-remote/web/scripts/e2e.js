// Runs the unit and browser tests, installing what they need first. Without
// a browser it skips (exit 0) locally, but fails loudly when CI is set.
//   ARCHON_BIN=/path/to/archon node scripts/e2e.js [playwright args…]
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const WEB = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const NPX = process.platform === 'win32' ? 'npx.cmd' : 'npx';
const NPM = process.platform === 'win32' ? 'npm.cmd' : 'npm';

function run(command, args) {
  const result = spawnSync(command, args, { cwd: WEB, stdio: 'inherit', env: process.env });
  if (result.error) throw new Error(`could not run ${command}: ${result.error.message}`, { cause: result.error });
  return result.status ?? 1;
}

function fail(message) {
  console.error(`e2e: ${message}`);
  process.exit(1);
}

if (!existsSync(join(WEB, 'node_modules', '@playwright', 'test'))) {
  if (run(NPM, ['ci', '--no-audit', '--no-fund']) !== 0) fail('npm ci failed');
}
// CI runners also need the browser's system libraries.
const withDeps = process.env.CI ? ['--with-deps'] : [];
if (run(NPX, ['playwright', 'install', ...withDeps, 'chromium']) !== 0) {
  if (process.env.CI) fail('the Chromium browser could not be installed, and CI is set');
  console.warn('e2e: SKIP: the Chromium browser could not be installed; set CI to make this a failure');
  process.exit(0);
}
if (run(process.execPath, ['--test']) !== 0) fail('unit tests failed');
process.exit(run(NPX, ['playwright', 'test', ...process.argv.slice(2)]));
