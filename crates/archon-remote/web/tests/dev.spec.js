// Every flow against a real archond through the dev bridge.
import { test as base, expect } from '@playwright/test';
import { FLOWS, openApp, ensureRepo } from './flows.js';
import { enqueue, notify } from './support/seed.js';
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

// Refuses the commands `refuse` names while it returns true for them.
async function refuseSome(page, refuse) {
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    ws.onMessage((message) => {
      const frame = JSON.parse(message);
      if (!refuse(frame)) return server.send(message);
      ws.send(JSON.stringify({ frame: 'reply', id: frame.req, result: { Err: { code: 'internal', message: `${frame.cmd} refused by the test` } } }));
    });
  });
}

test('load failures stay in view over what was loaded before', async ({ page, fixture }) => {
  let statusLoads = 0;
  await refuseSome(page, (frame) => ['feed_history', 'repo_list'].includes(frame.cmd) || (frame.cmd === 'status' && ++statusLoads > 1));
  const alert = (text) => page.getByRole('alert').filter({ hasText: text });
  const go = (name) => page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name }).click();
  await openApp(page, fixture, '#/inbox');
  const notifications = page.getByRole('region', { name: 'Notifications' });
  await expect(notifications.getByRole('alert')).toContainText('feed_history refused by the test');
  await expect(notifications.getByText('No notifications yet.')).toHaveCount(0);
  await go('Overview');
  await expect(alert('status refused by the test')).toBeVisible();
  await expect(page.locator('.hero-card')).toBeVisible();
  await go('Queue');
  await expect(alert('Could not load repositories: repo_list refused by the test')).toBeVisible();
  await page.locator('.item-row').first().click();
  await expect(page.locator('.item-hero')).toBeVisible();
  await expect(alert('Could not load the status: status refused by the test')).toBeVisible();
  await expect(alert('Could not load repositories')).toBeVisible();
  await go('Chat');
  await expect(alert('Could not load the status')).toBeVisible();
});

test('the new-item sheet shows a failed repository list and fills in once it loads', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  let refuseRepos = true;
  await refuseSome(page, (frame) => refuseRepos && frame.cmd === 'repo_list');
  await openApp(page, fixture);
  await page.getByRole('button', { name: 'New item' }).click();
  const sheet = page.getByRole('dialog', { name: 'New item' });
  const failed = sheet.getByRole('alert').filter({ hasText: 'Could not load repositories: repo_list refused by the test' });
  await expect(failed).toBeVisible();
  await sheet.getByLabel('Title').fill('Kept across the retry');
  refuseRepos = false;
  await failed.getByRole('button', { name: 'Try again' }).click();
  await expect(failed).toHaveCount(0);
  await expect(sheet.getByLabel('Repository')).toHaveValue('archon');
  await expect(sheet.getByLabel('Title')).toHaveValue('Kept across the retry');
});

test('a failed feed setup is retried until the app is live again', async ({ page, fixture }) => {
  let refuseFeed = true;
  await refuseSome(page, (frame) => refuseFeed && frame.cmd === 'feed_history');
  await openApp(page, fixture, '#/inbox');
  const notifications = page.getByRole('region', { name: 'Notifications' });
  await expect(notifications.getByRole('alert')).toContainText('feed_history refused by the test');
  refuseFeed = false;
  await expect(notifications.getByRole('alert')).toHaveCount(0, { timeout: 10_000 });
  const text = `Live again ${Date.now().toString(36)}`;
  await notify(fixture.archond, 'outcome', text);
  await expect(notifications.getByText(text)).toBeVisible();
});

test('a failed item reload stays in view when other data changes', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: `Reload me ${Date.now().toString(36)}`, brief: 'Its reload fails.' });
  const refuse = { item: false, status: false };
  let statusRefused = 0;
  await refuseSome(page, (frame) => {
    if (frame.cmd === 'status' && refuse.status) statusRefused += 1;
    return (frame.cmd === 'get_item' && refuse.item) || (frame.cmd === 'status' && refuse.status);
  });
  await openApp(page, fixture, `#/item/${item.id}`);
  await expect(page.getByRole('heading', { name: item.title, level: 1 })).toBeVisible();
  refuse.item = true;
  await notify(fixture.archond, 'progress', `Stage settled for ${item.id}.`, item.id);
  const failed = page.getByRole('alert').filter({ hasText: 'get_item refused by the test' });
  await expect(failed).toBeVisible();
  refuse.status = true;
  await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
  await expect.poll(() => statusRefused).toBeGreaterThan(0);
  await page.waitForTimeout(500);
  await expect(failed).toBeVisible();
  await expect(page.locator('.item-hero')).toHaveCount(0);
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
