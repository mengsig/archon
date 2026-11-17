// A connected tab whose browser pairs again as an observer in another tab hears of
// it and remounts as an observer, with no operator control and no reload.
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

test('a connected tab re-paired as an observer in another tab loses its operator controls', async ({ browser, fixture }) => {
  const known = new Set((await fixture.devices()).map((device) => device.id));
  const context = await browser.newContext();
  const page = await context.newPage();
  const problems = guardConsole(page);
  // Tab B opens the pairing form first: once the cookie is set, a new tab boots into the app.
  const other = await context.newPage();
  await other.goto(`${fixture.baseURL}/pair`);
  await expect(other.getByLabel('Pairing code')).toBeVisible();
  await pair(page, fixture, [], 'Playwright operator tab');
  const [operator] = (await fixture.devices()).filter((device) => !known.has(device.id));
  expect(operator?.role, 'tab A paired as an operator').toBe('operator');
  await page.goto(`${fixture.baseURL}/#/overview`);
  const newItem = page.getByRole('button', { name: 'New item', exact: true });
  await expect(newItem).toBeVisible();
  await expect(page.locator('.conn-banner')).toBeHidden();

  // Tab B shares tab A's cookie jar: its observer pairing replaces the cookie while A's socket stays up.
  const { code } = await fixture.remote(['pair', '--observer']);
  await other.getByLabel('Pairing code').fill(code);
  await other.getByLabel('Device name').fill('Playwright observer tab');
  await other.getByRole('button', { name: 'Pair' }).click();
  await expect(other.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  expect((await fixture.devices()).find((device) => device.id === operator.id)?.role, 'tab A\'s device stays paired').toBe('operator');
  await expect(newItem).toHaveCount(0, { timeout: 10_000 });
  await expect(page.getByRole('switch', { name: 'Pause new work' })).toHaveCount(0);
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  await expect(page.locator('.conn-banner')).toBeHidden({ timeout: 15_000 });
  await context.close();
  expect(problems, 'the page logged errors').toEqual([]);
});
