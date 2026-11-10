// The app behind a stand-in for Aether's `/archon` mount (support/mount.js):
// every asset, API call, the WebSocket, the cookie and the service worker
// must work under the prefix.
import { test as base, expect } from '@playwright/test';
import { FLOWS, openApp } from './flows.js';
import { startListenerFixture } from './support/listener.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startListenerFixture({ mounted: true });
    try {
      await use(fixture);
    } finally {
      await fixture.stop();
    }
  }, { scope: 'worker', timeout: 120_000 }],
  pairedState: [async ({ browser, fixture }, use) => {
    const context = await browser.newContext();
    const page = await context.newPage();
    await openApp(page, fixture);
    const state = await context.storageState();
    await context.close();
    await use(state);
  }, { scope: 'worker' }],
  storageState: async ({ pairedState }, use, testInfo) => {
    await use(testInfo.title.startsWith('unpaired') ? { cookies: [], origins: [] } : pairedState);
  },
  page: async ({ page }, use) => {
    const problems = guardConsole(page);
    await use(page);
    expect(problems, 'the page logged errors').toEqual([]);
  },
});

test.describe.configure({ mode: 'serial' });

test('unpaired: pair under /archon with the code from archon remote pair', async ({ page, fixture }) => {
  await FLOWS.pair(page, fixture);
  expect(new URL(page.url()).pathname).toBe('/archon/');
});

for (const name of ['viewQueueAndItem', 'chat', 'answerQuestion', 'followItemLive']) {
  test(`mounted: ${name}`, async ({ page, fixture }) => {
    await FLOWS[name](page, fixture);
  });
}

test('mounted: the cookie is scoped to /archon and the worker controls the prefix', async ({ page, fixture }) => {
  await openApp(page, fixture);
  const cookies = await page.context().cookies(fixture.baseURL);
  const device = cookies.find((cookie) => cookie.name === 'archon_device');
  expect(device?.path).toBe('/archon');
  expect(device?.httpOnly).toBe(true);
  const scope = await page.evaluate(async () => (await navigator.serviceWorker.ready).scope);
  expect(scope).toBe(`${fixture.baseURL}/`);
  await page.screenshot({ path: process.env.ARCHON_SHOT ?? 'test-results/mounted-queue.png' });
});
