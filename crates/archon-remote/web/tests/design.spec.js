// T-E1..T-E6 of the live design sessions plan, against a stand-in host
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
      await expect(page.getByLabel('Details the agent will receive')).toHaveValue(/Box: x \d+, y \d+, \d+ × \d+/);
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

  test('finishing: closing the review sheet leaves Review and Back to editing on the top bar', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      await page.getByRole('button', { name: 'Finish' }).click();
      await expect(page.getByRole('dialog', { name: 'Review before finishing' })).toBeVisible();
      await expect(page.locator('.sheet-layer :focus')).toHaveCount(1);
      await page.keyboard.press('Escape');
      await expect(page.getByRole('dialog', { name: 'Review before finishing' })).toHaveCount(0);
      await expect(page.getByRole('button', { name: 'Review', exact: true })).toBeVisible();
      await page.getByRole('button', { name: 'Review', exact: true }).click();
      await expect(page.getByRole('dialog', { name: 'Review before finishing' })).toBeVisible();
      expect(harness.daemon.state.requests.filter((r) => r.cmd === 'design_finish_capture').at(-1).recapture).toBe(false);
      await expect(page.locator('.sheet-layer :focus')).toHaveCount(1);
      await page.keyboard.press('Escape');
      await page.getByRole('button', { name: 'Back to editing' }).click();
      await expect(page.getByRole('button', { name: 'Finish' })).toBeVisible();
      await expect(page.getByRole('button', { name: 'Back to editing' })).toHaveCount(0);
      expect(harness.daemon.state.requests.some((r) => r.cmd === 'design_finish_cancel')).toBe(true);
      // One subscription for the whole run, however many session events came.
      expect(harness.daemon.state.requests.filter((r) => r.cmd === 'subscribe_design')).toHaveLength(1);
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

test.describe('robustness and accessibility', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('T-E5: preview down and back, a browser that keeps crashing, a dropped socket sends a note once', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      await expect(page.getByText('Live', { exact: true }).first()).toBeVisible();

      harness.daemon.pauseFrames();
      harness.daemon.pushSession({ degraded: true });
      await expect(page.getByRole('heading', { name: 'The app stopped responding' })).toBeVisible();
      await expect(page.getByText('Preview down', { exact: true }).first()).toBeVisible();
      harness.daemon.resumeFrames();
      await expect(page.getByRole('heading', { name: 'The app stopped responding' })).toHaveCount(0);
      await expect(page.getByText('Live', { exact: true }).first()).toBeVisible();

      harness.daemon.pushSession({ browser: { status: 'crashed_too_often' } });
      await expect(page.getByRole('heading', { name: 'The preview browser keeps stopping' })).toBeVisible();
      await expect(page.getByRole('button', { name: 'Reload' }).first()).toBeVisible();
      harness.daemon.pushSession({ browser: { status: 'running' } });
      await expect(page.getByRole('heading', { name: 'The preview browser keeps stopping' })).toHaveCount(0);

      await page.getByRole('button', { name: 'Note on the whole page' }).click();
      await page.getByLabel('What should change').fill('tighter spacing');
      harness.daemon.dropSockets();
      await page.getByLabel('What should change').press('Enter');
      await expect(page.locator('#note-D1').getByText('Done', { exact: true })).toBeVisible({ timeout: 20_000 });
      const adds = harness.daemon.state.requests.filter((r) => r.cmd === 'design_note_add');
      expect(new Set(adds.map((r) => r.client_key)).size).toBe(1);
      expect(harness.daemon.state.notes.map((n) => n.id)).toEqual(['D1']);
    } finally { await harness.stop(); }
  });

  test('T-E6: named controls, visible focus, a keyboard-only region note, polite announcements', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      const unnamed = await page.locator('.design').evaluate((root) => [...root.querySelectorAll('button, [role="radio"], textarea, input, select, [tabindex="0"]')]
        .filter((el) => el.getClientRects().length > 0)
        .filter((el) => {
          const by = el.getAttribute('aria-labelledby');
          const name = el.getAttribute('aria-label') || (by && document.getElementById(by)?.textContent) || el.textContent || el.title || [...(el.labels ?? [])].map((l) => l.textContent).join('');
          return !name.trim();
        })
        .map((el) => el.outerHTML.slice(0, 120)));
      expect(unnamed).toEqual([]);

      // Tab through the first stops: each shows a focus ring.
      for (let i = 0; i < 8; i += 1) {
        await page.keyboard.press('Tab');
        const ring = await page.evaluate(() => {
          const el = document.activeElement;
          const s = getComputedStyle(el);
          return { tag: el.tagName, outline: s.outlineStyle !== 'none' && parseFloat(s.outlineWidth) > 0, shadow: s.boxShadow !== 'none' };
        });
        if (ring.tag === 'BODY') continue;
        expect(ring.outline || ring.shadow, `focus ring on stop ${i} (${ring.tag})`).toBe(true);
      }

      await page.evaluate(() => {
        window.__said = [];
        const live = document.querySelector('.design-status [aria-live="polite"]');
        new MutationObserver(() => window.__said.push(live.textContent)).observe(live, { childList: true, characterData: true, subtree: true });
      });
      await page.locator('body').click({ position: { x: 5, y: 5 } });
      await page.keyboard.press('c');
      await expect(page.getByRole('radio', { name: 'Comment' })).toBeChecked();
      await page.keyboard.press('r');
      await expect(page.getByRole('radio', { name: 'Region' })).toBeChecked();
      await page.locator('.design-layer').focus();
      for (const key of ['ArrowRight', 'ArrowRight', 'ArrowDown', 'Shift+ArrowRight', 'Shift+ArrowDown']) await page.keyboard.press(key);
      await expect(page.locator('.design-region')).toBeVisible();
      await page.keyboard.press('Enter');
      await expect(page.getByText('What the agent will receive')).toBeVisible();
      await page.getByLabel('What should change').focus();
      await page.keyboard.type('more room here');
      await page.keyboard.press('Enter');
      await expect(page.locator('#note-D1').getByText('Done', { exact: true })).toBeVisible();
      await expect.poll(() => page.evaluate(() => window.__said.some((t) => /D1/.test(t)))).toBe(true);
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
