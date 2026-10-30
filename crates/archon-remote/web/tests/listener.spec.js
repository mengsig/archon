// Every flow against archond's real remote listener, paired with the code
// `archon remote pair` prints, and a revoke and a disable from the CLI.
// Claims are rate-limited (3 per 10 s), so most tests reuse one pairing.
import { test as base, expect } from '@playwright/test';
import { FLOWS, openApp } from './flows.js';
import { startListenerFixture } from './support/listener.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startListenerFixture();
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
    const unpaired = testInfo.title.startsWith('unpaired');
    await use(unpaired ? { cookies: [], origins: [] } : pairedState);
  },
  page: async ({ page }, use) => {
    const problems = guardConsole(page);
    await use(page);
    expect(problems, 'the page logged errors').toEqual([]);
  },
});

test.describe.configure({ mode: 'serial' });

test('unpaired: pair with the code from archon remote pair', async ({ page, fixture }) => {
  await FLOWS.pair(page, fixture);
});

for (const [name, flow] of Object.entries(FLOWS)) {
  if (name === 'pair') continue;
  test(`listener: ${name}`, async ({ page, fixture }) => {
    await flow(page, fixture);
  });
}

test('listener: the session cookie is HttpOnly and SameSite=Strict', async ({ page, fixture }) => {
  await openApp(page, fixture);
  const cookies = await page.context().cookies(fixture.baseURL);
  const device = cookies.find((cookie) => cookie.name === 'archon_device');
  expect(device, 'pairing set the device cookie').toBeTruthy();
  expect(device.httpOnly).toBe(true);
  expect(device.sameSite).toBe('Strict');
  expect(await page.evaluate(() => document.cookie)).not.toContain('archon_device');
});

test('unpaired: revoking the device from the CLI sends the app back to pairing', async ({ page, fixture }) => {
  const before = new Set((await fixture.devices()).map((device) => device.id));
  await openApp(page, fixture);
  const added = (await fixture.devices()).filter((device) => !before.has(device.id));
  expect(added, 'this browser paired exactly one device').toHaveLength(1);
  await fixture.remote(['revoke', added[0].id]);
  await expect(page.getByRole('form', { name: 'Pair this device' })).toBeVisible({ timeout: 15_000 });
  const status = await page.request.get(`${fixture.baseURL}/api/session`);
  expect(status.status()).toBe(401);
});

// Last: it closes the listener for the rest of this worker.
test('listener: disabling remote access closes the app and the port', async ({ page, fixture }) => {
  await openApp(page, fixture);
  const off = await fixture.remote(['disable']);
  expect(off.listening).toBeNull();
  await expect(page.locator('.conn-banner')).toContainText('Remote access is off', { timeout: 15_000 });
  await expect(page.request.get(`${fixture.baseURL}/api/health`)).rejects.toThrow();
});
