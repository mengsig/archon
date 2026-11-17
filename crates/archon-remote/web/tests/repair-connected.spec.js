// A connected tab whose browser pairs again as an observer in another tab hears of
// it and remounts as an observer, with no operator control and no reload.
import { test as base, expect } from '@playwright/test';
import { startListenerFixture } from './support/listener.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  // One listener per test: each pairs twice, and claims are rate-limited (3 per 10 s).
  fixture: [async ({}, use) => {
    const fixture = await startListenerFixture();
    try {
      await use(fixture);
    } finally {
      await fixture.stop();
    }
  }, { scope: 'test', timeout: 120_000 }],
});

async function pair(page, fixture, args, name) {
  const { code } = await fixture.remote(['pair', ...args]);
  await page.goto(`${fixture.baseURL}/pair#code=${code}`);
  await page.getByLabel('Device name').fill(name);
  await page.getByRole('button', { name: 'Pair' }).click();
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
}

// Tab A is an operator with its socket up; tab B re-pairs the shared cookie as an observer.
// `beforeRepair` runs on tab A just before and may return a step run once tab B paired;
// the result is tab A's console problems.
async function repairAsObserver(browser, fixture, beforeRepair = async () => {}) {
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

  const afterRepair = await beforeRepair(page);
  // Tab B shares tab A's cookie jar: its observer pairing replaces the cookie while A's socket stays up.
  const { code } = await fixture.remote(['pair', '--observer']);
  await other.getByLabel('Pairing code').fill(code);
  await other.getByLabel('Device name').fill('Playwright observer tab');
  await other.getByRole('button', { name: 'Pair' }).click();
  await expect(other.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  await afterRepair?.();
  expect((await fixture.devices()).find((device) => device.id === operator.id)?.role, 'tab A\'s device stays paired').toBe('operator');
  await expect(newItem).toHaveCount(0, { timeout: 10_000 });
  await expect(page.getByRole('switch', { name: 'Pause new work' })).toHaveCount(0);
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  await expect(page.locator('.conn-banner')).toBeHidden({ timeout: 15_000 });
  await context.close();
  return problems;
}

const busy = (problem) => /status of 503/.test(problem) && /\/api\/session/.test(problem);

test('a connected tab re-paired as an observer in another tab loses its operator controls', async ({ browser, fixture }) => {
  expect(await repairAsObserver(browser, fixture), 'the page logged errors').toEqual([]);
});

test('a connected tab whose first session read after the re-pair fails retries and remounts', async ({ browser, fixture }) => {
  let refused = 0;
  const problems = await repairAsObserver(browser, fixture, async (page) => {
    await page.route('**/api/session', async (route) => {
      if (refused > 0) return route.fallback();
      refused += 1;
      await route.fulfill({ status: 503, contentType: 'application/json', headers: { 'Retry-After': '1' },
        body: JSON.stringify({ error: { code: 'busy', message: 'Every connection slot is taken.' } }) });
    });
  });
  expect(refused, 'the first session read after the re-pair was refused').toBe(1);
  expect(problems.filter((problem) => !busy(problem)), 'the page logged errors').toEqual([]);
});

test('a connected tab that cannot confirm its session withholds even an open sheet until it can', async ({ browser, fixture }) => {
  let refused = 0;
  const problems = await repairAsObserver(browser, fixture, async (page) => {
    await page.getByRole('button', { name: 'New item', exact: true }).click();
    await expect(page.getByRole('dialog')).toBeVisible();
    await page.route('**/api/session', (route) => {
      refused += 1;
      return route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { code: 'busy' } }) });
    });
    return async () => {
      await expect(page.locator('.conn-banner')).toContainText('Checking session…', { timeout: 15_000 });
      await expect(page.locator('#overlay')).toHaveJSProperty('inert', true);
      await expect(page.locator('#views')).toHaveJSProperty('inert', true);
      await page.unroute('**/api/session');
      // A page shown again checks at once rather than waiting out the backoff.
      await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
    };
  });
  expect(refused, 'the session reads kept being refused').toBeGreaterThan(3);
  expect(problems.filter((problem) => !busy(problem)), 'the page logged errors').toEqual([]);
});
