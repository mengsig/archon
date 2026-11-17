// A tab whose device is revoked and re-paired as an observer in another tab
// while its socket is down comes back as an observer, with no operator control.
import { test as base, expect } from '@playwright/test';
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
});

async function pair(page, fixture, args, name) {
  const { code } = await fixture.remote(['pair', ...args]);
  await page.goto(`${fixture.baseURL}/pair#code=${code}`);
  await page.getByLabel('Device name').fill(name);
  await page.getByRole('button', { name: 'Pair' }).click();
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
}

// While `down`, the tab's socket and session check are cut, as on a lost network.
async function cuttable(page) {
  const link = { down: false, socket: null };
  await page.routeWebSocket(/\/ws$/, (ws) => {
    if (link.down) return ws.close({ code: 1001, reason: 'cut by the test' });
    ws.connectToServer();
    link.socket = ws;
  });
  await page.route('**/api/session', (route) => (link.down ? route.abort() : route.continue()));
  return link;
}

test('a tab re-paired as an observer during a disconnect loses its operator controls', async ({ browser, fixture }) => {
  const known = new Set((await fixture.devices()).map((device) => device.id));
  const tab = await browser.newContext();
  const page = await tab.newPage();
  const problems = guardConsole(page);
  const link = await cuttable(page);
  await pair(page, fixture, [], 'Playwright tab');
  const [operator] = (await fixture.devices()).filter((device) => !known.has(device.id));
  expect(operator?.role, 'the tab paired as an operator').toBe('operator');
  await page.goto(`${fixture.baseURL}/#/overview`);
  const newItem = page.getByRole('button', { name: 'New item', exact: true });
  await expect(newItem).toBeVisible();

  const other = await browser.newContext();
  await pair(await other.newPage(), fixture, ['--observer'], 'Playwright other tab');
  link.down = true;
  link.socket.close({ code: 1001, reason: 'cut by the test' });
  await expect(page.locator('.conn-banner')).toBeVisible({ timeout: 10_000 });
  await fixture.remote(['revoke', operator.id]);
  // Tabs share one cookie jar: the other tab's pairing replaced this tab's cookie.
  await tab.clearCookies();
  await tab.addCookies(await other.cookies());
  link.down = false;

  await expect(newItem).toHaveCount(0, { timeout: 20_000 });
  await expect(page.getByRole('switch', { name: 'Pause new work' })).toHaveCount(0);
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  await expect(page.locator('.conn-banner')).toBeHidden({ timeout: 15_000 });
  await other.close();
  await tab.close();
  // The browser logs the session checks the test itself cut.
  const unexpected = problems.filter((problem) => !/ERR_FAILED.*\/api\/session/.test(problem));
  expect(unexpected, 'the page logged errors').toEqual([]);
});
