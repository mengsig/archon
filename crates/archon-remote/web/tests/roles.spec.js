// An observer device reads every screen without a single control that
// changes Archon; an operator keeps them all. The CLI's pause reaches both.
import { test as base, expect } from '@playwright/test';
import { startListenerFixture } from './support/listener.js';
import { guardConsole } from './support/console-guard.js';

const VIEWPORTS = [{ width: 390, height: 844 }, { width: 1280, height: 800 }];

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

test.describe.configure({ mode: 'serial' });

// Pairing is rate-limited, so each role pairs once and later pages reuse
// its session cookie through the saved storage state.
const sessions = new Map();

async function pairedPage(browser, fixture, role, viewport) {
  const saved = sessions.get(role);
  const context = await browser.newContext({ viewport, storageState: saved });
  const page = await context.newPage();
  const problems = guardConsole(page);
  if (!saved) {
    const { code } = await fixture.remote(role === 'observer' ? ['pair', '--observer'] : ['pair']);
    await page.goto(`${fixture.baseURL}/pair#code=${code}`);
    await page.getByLabel('Device name').fill(`Playwright ${role}`);
    await page.getByRole('button', { name: 'Pair' }).click();
    await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
    sessions.set(role, await context.storageState());
  }
  const close = async () => {
    await context.close();
    expect(problems, 'the page logged errors').toEqual([]);
  };
  return { page, close };
}

// Each screen with what proves its data has rendered, so absence means hidden.
function screens(fixture) {
  const first = fixture.seeded.first;
  return [
    ['#/overview', (page) => page.locator('.hero-label')],
    ['#/queue', (page) => page.getByText('Add a remote listener for the phone app').first()],
    ['#/inbox', (page) => page.getByText('Ship the listener behind a feature flag first?')],
    ['#/chat', (page) => page.getByRole('log', { name: 'Conversation' })],
    [`#/item/${first.id}`, (page) => page.getByRole('heading', { level: 1, name: 'Add a remote listener for the phone app' })],
    ['#/graph', (page) => page.locator('.graph-node').first()],
    ['#/settings', (page) => page.getByRole('heading', { name: 'Settings', level: 1 })],
  ];
}

function operatorControls(page) {
  return {
    'Pause new work': page.getByRole('switch', { name: 'Pause new work' }),
    'New item': page.getByRole('button', { name: 'New item', exact: true }),
    'question options': page.locator('.option-button'),
    'Send answer': page.getByRole('button', { name: 'Send answer' }),
    'Answer box': page.getByRole('textbox', { name: /^Answer Q/ }),
    Commands: page.getByRole('button', { name: 'Commands', exact: true }),
    'item actions': page.getByRole('group', { name: 'Item actions' }),
    Merge: page.getByRole('button', { name: 'Merge', exact: true }),
    'Cancel or Retry': page.getByRole('button', { name: /^(Cancel|Retry)$/ }),
    'Pause or Resume': page.getByRole('button', { name: /^(Pause|Resume) / }),
    Message: page.getByRole('textbox', { name: 'Message' }),
    Send: page.getByRole('button', { name: 'Send', exact: true }),
  };
}

async function visit(page, fixture, hash, ready) {
  await page.goto(`${fixture.baseURL}/${hash}`);
  await expect(ready(page)).toBeVisible();
}

for (const viewport of VIEWPORTS) {
  test(`observer at ${viewport.width}px sees no operator control on any screen`, async ({ browser, fixture }) => {
    const { page, close } = await pairedPage(browser, fixture, 'observer', viewport);
    for (const [hash, ready] of screens(fixture)) {
      await visit(page, fixture, hash, ready);
      for (const [name, control] of Object.entries(operatorControls(page))) {
        await expect(control, `${name} on ${hash}`).toHaveCount(0);
      }
      expect(await page.locator('main').innerText(), `a hidden control printed on ${hash}`).not.toMatch(/\bnull\b/);
    }
    await visit(page, fixture, '#/inbox', screens(fixture)[2][1]);
    await expect(page.getByText('Options: approve · reject')).toBeVisible();
    await close();
  });

  test(`operator at ${viewport.width}px keeps every operator control`, async ({ browser, fixture }) => {
    const { page, close } = await pairedPage(browser, fixture, 'operator', viewport);
    const [overview, queue, inbox, chat, item] = screens(fixture);
    await visit(page, fixture, ...overview);
    await expect(page.getByRole('switch', { name: 'Pause new work' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'New item', exact: true })).toBeVisible();
    await expect(page.getByRole('button', { name: /^Pause archon$/ })).toBeVisible();
    await visit(page, fixture, ...queue);
    await expect(page.getByRole('button', { name: 'New item', exact: true })).toBeVisible();
    await visit(page, fixture, ...inbox);
    await expect(page.locator('.option-button', { hasText: 'approve' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Send answer' }).first()).toBeVisible();
    await visit(page, fixture, ...chat);
    await expect(page.getByRole('textbox', { name: 'Message' })).toBeVisible();
    await expect(page.getByRole('button', { name: 'Commands', exact: true })).toBeVisible();
    await visit(page, fixture, ...item);
    const actions = page.getByRole('group', { name: 'Item actions' });
    await expect(actions.getByRole('button', { name: 'Cancel', exact: true })).toBeVisible();
    await expect(actions.getByRole('button', { name: /^Pause repo archon/ })).toBeVisible();
    await close();
  });
}

// No change announces a pause: the shown Overview reads status every 5 s.
test('the CLI pause and resume reach the open overview within the 5 s status poll', async ({ browser, fixture }) => {
  const { page, close } = await pairedPage(browser, fixture, 'operator', VIEWPORTS[0]);
  await visit(page, fixture, '#/overview', (p) => p.locator('.hero-label'));
  const toggle = page.getByRole('switch', { name: 'Pause new work' });
  await expect(toggle).not.toBeChecked();
  await fixture.archond.run(['pause']);
  await expect(page.locator('.hero-label')).toHaveText('Paused', { timeout: 6_500 });
  await expect(toggle).toBeChecked();
  await fixture.archond.run(['resume']);
  await expect(page.locator('.hero-label')).not.toHaveText('Paused', { timeout: 6_500 });
  await expect(toggle).not.toBeChecked();
  await close();
});

for (const viewport of VIEWPORTS) {
  test(`settings at ${viewport.width}px is the current section, not Overview`, async ({ browser, fixture }) => {
    const { page, close } = await pairedPage(browser, fixture, 'observer', viewport);
    await visit(page, fixture, '#/settings', (p) => p.getByRole('heading', { name: 'Settings', level: 1 }));
    const status = await fixture.archond.request({ cmd: 'status' });
    const app = page.locator('.row').filter({ has: page.getByText('App', { exact: true }) });
    await expect(app.locator('.row-value')).toHaveText(status.data.daemon_version);
    const nav = page.getByRole('navigation', { name: 'Sections' });
    for (const name of ['Overview', 'Queue', 'Graph', 'Inbox', 'Chat']) {
      await expect(nav.getByRole('link', { name: new RegExp(`^${name}`) })).not.toHaveAttribute('aria-current', 'page');
    }
    await expect(page.locator('a.nav-settings')).toHaveAttribute('aria-current', 'page');
    if (viewport.width >= 1280) await expect(page.getByRole('link', { name: 'Settings', exact: true })).toHaveAttribute('aria-current', 'page');
    await close();
  });
}
