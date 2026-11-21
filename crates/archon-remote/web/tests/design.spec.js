// T-E1..T-E4 of the live design sessions plan, against a stand-in host
// (support/design-harness.js): the real web app and the fixture app in
// fixtures/design-app, with a scripted daemon. A real archond run of the same
// flows belongs to the daemon tasks' own e2e.
import { test as base, expect } from '@playwright/test';
import { startHarness, ITEM } from './support/design-harness.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  page: async ({ page }, use) => {
    const problems = guardConsole(page);
    await use(page);
    expect(problems, 'the page logged errors').toEqual([]);
  },
});
test.describe.configure({ mode: 'serial' });

async function open(browser, page, options) {
  const harness = await startHarness(browser, options);
  await harness.install(page);
  await page.goto(`${harness.baseURL}/#/design/${ITEM}`);
  return harness;
}

// The canvas pixel under the fixture's button, to see the frame change.
const buttonPixel = async (page, harness) => page.evaluate((box) => {
  const canvas = document.querySelector('.design-canvas');
  const [r, g, b] = canvas.getContext('2d').getImageData(Math.round(box.x + box.width / 2), Math.round(box.y + 4), 1, 1).data;
  return [r, g, b];
}, await harness.appPage.locator('#cta').boundingBox());

test.describe('desktop', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('T-E1: comment, send, watch it turn green, compare, undo, finish', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      const canvas = page.locator('.design-canvas');
      await expect(canvas).toBeVisible();
      await expect(page.getByText('Live', { exact: true }).first()).toBeVisible();
      await expect(page.getByText('Click Comment, then click anything in your app to ask for a change.')).toBeVisible();
      await expect.poll(async () => (await buttonPixel(page, harness))[2]).toBeGreaterThan(150); // blue to start

      await page.getByRole('radio', { name: 'Comment' }).click();
      const box = await harness.appPage.locator('#cta').boundingBox();
      const rect = await canvas.boundingBox();
      const scale = rect.width / 800;
      await page.mouse.click(rect.x + (box.x + box.width / 2) * scale, rect.y + (box.y + box.height / 2) * scale);
      await expect(page.getByText('What the agent will receive')).toBeVisible();
      await expect(page.getByLabel('Details the agent will receive')).toHaveValue(/Selector: main > button#cta.primary/);
      await page.getByLabel('What should change').fill('make this green');
      await page.getByLabel('What should change').press('Enter');

      const first = page.locator('#note-D1');
      await expect(first.getByText('Done', { exact: true })).toBeVisible();
      await expect.poll(async () => (await buttonPixel(page, harness))[1], { timeout: 8000 }).toBeGreaterThan(110); // now green
      await first.getByRole('button', { name: 'Compare' }).click();
      await expect(page.getByText(/captured from the preview at commit abc1234/).first()).toBeVisible();
      await page.getByRole('button', { name: 'Close' }).click();
      await expect(page.getByRole('dialog')).toHaveCount(0);

      // A second note, then undo it.
      await page.getByRole('button', { name: 'Note on the whole page' }).click();
      await page.getByLabel('What should change').fill('add a heading');
      await page.getByLabel('What should change').press('Enter');
      await expect(page.locator('#note-D2').getByText('Done', { exact: true })).toBeVisible();
      await page.locator('#note-D2').getByRole('button', { name: 'Undo' }).click();
      await expect(page.getByText('Undo D2? Archon will reverse its change.')).toBeVisible();
      await page.getByRole('button', { name: 'Undo D2' }).click();
      await expect(page.locator('#note-D2').getByText('Undone', { exact: true })).toBeVisible();

      await page.getByRole('button', { name: 'Finish' }).click();
      await expect(page.getByRole('dialog', { name: 'Review before finishing' })).toBeVisible();
      await expect(page.getByLabel('Include picture for D1')).toBeVisible();
      await expect(page.getByLabel('Include picture for D2')).toHaveCount(0);
      await page.getByRole('button', { name: 'Confirm' }).click();
      await expect.poll(() => harness.daemon.state.confirmed?.include).toEqual({ D1: true });
    } finally { await harness.stop(); }
  });

  test('T-E4: a frozen old tab learns its session ended and sends nothing more', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      harness.daemon.endSession();
      await expect(page.getByRole('heading', { name: 'This design session ended' })).toBeVisible();
      const before = harness.daemon.state.requests.length;
      await page.keyboard.press('c');
      await page.waitForTimeout(300);
      expect(harness.daemon.state.requests.slice(before).filter((r) => r.cmd === 'design_note_add')).toEqual([]);
    } finally { await harness.stop(); }
  });
});

test.describe('phone', () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test('T-E2: bottom bar, notes sheet, tap-select, Wider, send, status updates', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      const bar = page.getByRole('navigation', { name: 'Design modes' });
      for (const name of ['Use', 'Comment', 'Region', 'Notes']) {
        const b = await bar.getByRole('button', { name }).boundingBox();
        expect(b.height).toBeGreaterThanOrEqual(44);
        expect(b.width).toBeGreaterThanOrEqual(44);
      }
      await bar.getByRole('button', { name: 'Comment' }).click();
      const box = await harness.appPage.locator('#cta').boundingBox();
      const rect = await page.locator('.design-canvas').boundingBox();
      const scale = rect.width / 800;
      await page.touchscreen.tap(rect.x + (box.x + box.width / 2) * scale, rect.y + (box.y + box.height / 2) * scale);
      await expect(page.getByText('What the agent will receive')).toBeVisible();
      await page.getByRole('button', { name: /Wider selection/ }).click();
      await expect.poll(() => harness.daemon.state.requests.filter((r) => r.cmd === 'design_resolve' && r.select).map((r) => r.depth)).toContain(1);
      await page.getByLabel('What should change').fill('bigger');
      await page.getByRole('button', { name: 'Send' }).click();
      await expect(page.locator('#note-D1').getByText('Done', { exact: true })).toBeVisible();
    } finally { await harness.stop(); }
  });
});

test('T-E3: an observer sees the status line only and sends no design request', async ({ browser, page }) => {
  const harness = await open(browser, page, { role: 'observer' });
  try {
    await expect(page.getByText('Only the operator’s devices see the live preview.')).toBeVisible();
    await expect(page.locator('.design-canvas')).toBeHidden();
    const cmds = harness.daemon.state.requests.map((r) => r.cmd).filter((c) => c?.includes('design'));
    expect(cmds).toEqual(['design_get']);
  } finally { await harness.stop(); }
});
