// Every flow against a real archond through the dev bridge.
import { test as base, expect } from '@playwright/test';
import { FLOWS, openApp } from './flows.js';
import { enqueue } from './support/seed.js';
import { startDevFixture } from './support/fixture.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startDevFixture();
    try {
      await use(fixture);
    } finally {
      await fixture.stop();
    }
  }, { scope: 'worker', timeout: 120_000 }],
  page: async ({ page }, use) => {
    const problems = guardConsole(page);
    await use(page);
    expect(problems, 'the page logged errors').toEqual([]);
  },
});

for (const [name, flow] of Object.entries(FLOWS)) {
  test(name, async ({ page, fixture }) => {
    await flow(page, fixture);
  });
}

// Behaviour the bridge can act out but a real listener cannot be asked to.
test('reconnects and resubscribes after the socket drops', async ({ page, fixture }) => {
  await openApp(page, fixture);
  await expect(page.locator('.item-row').first()).toBeVisible();
  fixture.dev.closeClients(1012, 'restarting');
  const title = `Arrived while reconnecting ${Date.now().toString(36)}`;
  await enqueue(fixture.archond, { repo: 'archon', title, brief: 'Shows up once the app is back.' });
  await expect(page.getByRole('link', { name: new RegExp(`^${title}`) })).toBeVisible({ timeout: 20_000 });
  await expect(page.locator('.conn-banner')).not.toHaveClass(/is-shown/);
});

test('a revoked device goes back to pairing', async ({ page, fixture }) => {
  await openApp(page, fixture);
  fixture.dev.closeClients(4401, 'revoked');
  await expect(page.getByRole('form', { name: 'Pair this device' })).toBeVisible();
});

test('the theme override sticks and reduced motion stills the page', async ({ page, fixture }) => {
  await page.emulateMedia({ colorScheme: 'light', reducedMotion: 'reduce' });
  await openApp(page, fixture, '#/settings');
  await page.getByRole('radio', { name: 'Dark' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.reload();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name: 'Queue' }).click();
  const duration = await page.locator('.view').first().evaluate((el) => getComputedStyle(el).animationDuration);
  expect(Number.parseFloat(duration)).toBeLessThan(0.01);
});
