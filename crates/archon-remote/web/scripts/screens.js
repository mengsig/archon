// Regenerates docs/assets/remote/*.png from a seeded archond's own listener.
//   ARCHON_BIN=/path/to/archon node scripts/screens.js
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from '@playwright/test';
import { startListenerFixture } from '../tests/support/listener.js';
import { captureScreens } from '../tests/screens.js';
import { guardConsole } from '../tests/support/console-guard.js';

const OUT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..', '..', 'docs', 'assets', 'remote');

const fixture = await startListenerFixture();
const browser = await chromium.launch();
try {
  for (const [name, width, height] of [['phone', 390, 844], ['desktop', 1280, 800]]) {
    const context = await browser.newContext({ viewport: { width, height }, deviceScaleFactor: 2, hasTouch: name === 'phone' });
    const page = await context.newPage();
    const problems = guardConsole(page);
    await captureScreens(page, fixture, name === 'phone' ? OUT : resolve(OUT, name));
    if (problems.length) throw new Error(`the page logged errors:\n${problems.join('\n')}`);
    await context.close();
  }
  console.log(`wrote screens to ${OUT}`);
} finally {
  await browser.close();
  await fixture.stop();
}
