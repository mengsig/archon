// Captures every screen in light and dark at the page's viewport, as
// <screen>-<light|dark>.png. Expects a fresh, unpaired browser context.
import { mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { expect } from '@playwright/test';
import { openApp } from './flows.js';
import { notify } from './support/seed.js';

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
  await openApp(page, fixture);
  await seedConversation(fixture);

  await shoot('queue', () => expect(page.locator('.item-row').first()).toBeVisible());
  await page.locator('.item-row').first().click();
  await shoot('item', () => expect(page.getByRole('heading', { name: 'Stages' })).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/inbox`);
  await shoot('inbox', () => expect(page.locator('.question-card').first()).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/chat`);
  await shoot('chat', async () => {
    await expect(page.locator('.bubble-row').first()).toBeVisible();
    await page.evaluate(() => window.scrollTo(0, document.documentElement.scrollHeight));
  });
  await page.goto(`${fixture.baseURL}/#/overview`);
  await shoot('overview', () => expect(page.locator('.hero-card')).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/settings`);
  await shoot('settings', () => expect(page.getByRole('button', { name: 'Forget this device' })).toBeVisible());
  await page.goto(`${fixture.baseURL}/#/queue`);
  await page.getByRole('button', { name: 'New item' }).click();
  await shoot('new-item', () => expect(page.getByRole('dialog', { name: 'New item' })).toBeVisible());
}

// Operator lines are recorded even while Aether is down (say answers unavailable).
async function seedConversation(fixture) {
  for (const text of ['Morning. What is left before the remote listener can ship?', 'Keep the pairing code short, eight characters is fine.']) {
    try {
      await fixture.archond.request({ cmd: 'say', text });
    } catch (err) {
      if (err.code !== 'unavailable') throw err;
    }
  }
  await notify(fixture.archond, 'outcome', 'The listener design is **approved**. Next in line:\n\n- the `pairing` flow\n- a [web app](https://example.com) pass');
}
