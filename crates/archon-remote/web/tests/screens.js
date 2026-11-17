// Captures every screen in light and dark at the page's viewport, as
// <screen>-<light|dark>.png. Expects a fresh, unpaired browser context.
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { openApp } from './flows.js';
import { notify } from './support/seed.js';
import { pipelineFixture, standInGraphs } from './support/graph-fixture.js';

const THEMES = ['light', 'dark'];

export async function captureScreens(page, fixture, outDir) {
  mkdirSync(outDir, { recursive: true });
  await page.emulateMedia({ reducedMotion: 'reduce' });
  const shoot = async (screen, ready) => {
    for (const theme of THEMES) {
      await page.emulateMedia({ colorScheme: theme, reducedMotion: 'reduce' });
      await ready();
      await page.evaluate(() => document.fonts.ready);
      await page.screenshot({ path: join(outDir, `${screen}-${theme}.png`), animations: 'disabled', caret: 'hide' });
    }
  };

  await page.goto(`${fixture.baseURL}/`);
  await shoot('pairing', () => expect(page.getByRole('form', { name: 'Pair this device' })).toBeVisible());
  const base = (await fixture.archond.request({ cmd: 'get_item', item: fixture.seeded.first.id })).data;
  await standInGraphs(page, pipelineFixture(base), CHAT_ENTRIES);
  await openApp(page, fixture);
  await seedConversation(fixture);

  await shoot('queue', () => expect(page.locator('.item-row').first()).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/item/${fixture.seeded.first.id}`);
  await shoot('item', () => expect(page.getByRole('heading', { name: 'Stages' })).toBeVisible());
  await page.getByRole('tab', { name: 'Work', exact: true }).click();
  await shoot('work', () => expect(page.locator('#item-work .graph-node')).toHaveCount(10));
  await page.getByRole('tab', { name: 'Agents', exact: true }).click();
  await shoot('agents', () => expect(page.locator('#item-agents .graph-node')).toHaveCount(7));
  await page.goto(`${fixture.baseURL}/#/graph`);
  await shoot('graph', () => expect(page.locator('.graph-node').first()).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/inbox`);
  await shoot('inbox', () => expect(page.locator('.question-card').first()).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/chat`);
  await shoot('chat', async () => {
    await expect(page.locator('.bubble-row').first()).toBeVisible();
    await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  });
  await page.getByRole('button', { name: 'Commands', exact: true }).click();
  await shoot('commands', () => expect(page.getByRole('dialog', { name: 'Operator commands' })).toBeVisible());
  await page.getByRole('button', { name: 'Close', exact: true }).click();
  await page.goto(`${fixture.baseURL}/#/overview`);
  await shoot('overview', () => expect(page.locator('.hero-card')).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/settings`);
  await shoot('settings', () => expect(page.getByRole('button', { name: 'Forget this device' })).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/queue`);
  await page.getByRole('button', { name: 'New item' }).click();
  await shoot('new-item', () => expect(page.getByRole('dialog', { name: 'New item' })).toBeVisible());
}

const CHAT_ENTRIES = [
  { kind: 'operator', text: 'Morning. What is left before the remote listener can ship?' },
  { kind: 'assistant', text: 'Two things: the **pairing** flow and a pass over the web app.' },
  { kind: 'post', text: 'The listener design is **approved**.', links: ['https://example.com'] },
  { kind: 'question', text: 'Is an eight character pairing code short enough?' },
  { kind: 'operator', text: 'Keep the pairing code short, eight characters is fine.' },
];

async function seedConversation(fixture) {
  await notify(fixture.archond, 'outcome', 'The listener design is **approved**. Next in line:\n\n- the `pairing` flow\n- a [web app](https://example.com) pass');
}
