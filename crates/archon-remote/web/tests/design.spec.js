// T-E1..T-E6 of the live design sessions plan, against a stand-in host
// (support/design-harness.js): the real web app and the fixture app in
// fixtures/design-app, with a scripted daemon. A real archond run of the same
// flows belongs to the daemon tasks' own e2e.
import { test as base, expect } from '@playwright/test';
import { mkdirSync } from 'node:fs';
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
const EVIDENCE = new URL('../../../../.archon/evidence/web', import.meta.url).pathname;
mkdirSync(EVIDENCE, { recursive: true });

async function open(browser, page, options) {
  const harness = await startHarness(browser, options);
  await harness.install(page);
  await page.goto(`${harness.baseURL}/#/design/${ITEM}`);
  return harness;
}

// A note D1 the agent has already done, for a session opened without one.
function doneNote(harness) {
  harness.daemon.state.notes.unshift({ item: ITEM, id: 'D1', session: 'ds_FIXTURE', kind: 'change', text: 'make it green', details_text: null, send_picture: false, target: null, viewport: { w: 800, h: 500, preset: 'desktop' },
    status: 'done', fail_reason: null, base_commit: 'base0001', commit: 'abc1234def', no_change: false, reverts: null, resent_from: null, superseded_by: null, thread: [], delivery: 0,
    before_asset: null, after_asset: null, created_at: new Date().toISOString(), updated_at: new Date().toISOString() });
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
    const gate = { id: 'q_GATE', item: ITEM, number: 1, text: 'Approve the design?', options: ['approve', 'revise', 'reject'], asked_at: new Date().toISOString(), answer: null };
    const harness = await open(browser, page, { questions: [gate] });
    try {
      const canvas = page.locator('.design-canvas');
      await expect(canvas).toBeVisible();
      await expect(page.getByText('Live', { exact: true }).first()).toBeVisible();
      await expect(page.getByText('Click Comment, then click anything in your app to ask for a change.')).toBeVisible();
      await expect.poll(async () => (await buttonPixel(page, harness))[2]).toBeGreaterThan(150); // blue to start

      await page.getByRole('radio', { name: 'Comment' }).click();
      const box = await harness.appPage.locator('#cta').boundingBox();
      const rect = await canvas.boundingBox();
      const scale = rect.width / 390;
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
      await expect.poll(() => harness.daemon.state.confirmed?.include).toEqual({});

      // Confirmed is not ended: the screen waits for the operator's approval.
      await expect(page.getByRole('heading', { name: 'Waiting for your approval' }).first()).toBeVisible();
      await expect(page.getByText('This design session ended')).toHaveCount(0);
      const ledger = page.getByRole('region', { name: 'Ledger and approval' });
      await expect(ledger.locator('.ledger-row')).toHaveCount(2);
      await expect(ledger.locator('.ledger-row').first()).toContainText('make this green');
      // Each note's card shows its picture from before and the one from after.
      for (const row of await ledger.locator('.ledger-row').all()) {
        await expect(row.locator('.ledger-pic .cmp-caption')).toHaveText(['Before', 'After']);
        await expect(row.locator('.ledger-img')).toHaveCount(2);
      }
      await expect(page.getByRole('dialog')).toHaveCount(0);
      await page.waitForTimeout(500);
      await page.screenshot({ path: `${EVIDENCE}/confirmed-approval.png` });
      await ledger.getByRole('button', { name: 'Ask for changes' }).click();
      await expect(ledger.getByRole('alert')).toContainText('Say what should change');
      await ledger.getByLabel('What to change').fill('make it bigger');
      await ledger.getByRole('button', { name: 'Ask for changes' }).click();
      await expect.poll(() => harness.daemon.state.requests.find((r) => r.cmd === 'answer')).toMatchObject({ question: 'q_GATE', text: 'revise: make it bigger' });
    } finally { await harness.stop(); }
  });

  test('a done note whose pictures match shows the notice, not two identical pictures', async ({ browser, page }) => {
    const stamp = new Date().toISOString();
    const note = (id, fields) => ({ item: ITEM, id, session: 'ds_FIXTURE', kind: 'change', text: `change ${id}`, details_text: null, send_picture: false, target: { path: '/', selector: 'button', role_name: null, component: null, box: { x: 0, y: 0, w: 10, h: 10 }, viewport: { w: 800, h: 500, preset: 'desktop' }, styles: {}, region: false },
      viewport: { w: 800, h: 500, preset: 'desktop' }, status: 'done', fail_reason: null, base_commit: 'base0001', commit: 'abc1234def', no_change: false, reverts: null, resent_from: null, superseded_by: null, thread: [], delivery: 1,
      before_asset: 'da_b', after_asset: null, final_asset: null, final_included: null, created_at: stamp, updated_at: stamp, ...fields });
    const harness = await open(browser, page, { notes: [
      note('D3', { thread: [{ at: stamp, from: 'archon', text: 'No visible change was captured: Only the hover colour changed.' }] }),
      note('D2', { after_asset: 'da_a' }),
    ] });
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      await page.locator('#note-D3').getByRole('button', { name: 'Compare' }).click();
      const dialog = page.getByRole('dialog');
      await expect(dialog.getByText('No visible change captured')).toBeVisible();
      await expect(dialog).toContainText('Only the hover colour changed.');
      await expect(dialog.locator('img.cmp-img')).toHaveCount(1); // the before picture alone
      await expect(dialog.locator('.cmp-range')).toHaveCount(0);
      await page.getByRole('button', { name: 'Close' }).click();
      // A note that did change still compares its two pictures.
      await page.locator('#note-D2').getByRole('button', { name: 'Compare' }).click();
      await expect(page.getByRole('dialog').locator('img.cmp-img')).toHaveCount(2);
      await expect(page.getByRole('dialog')).not.toContainText('No visible change captured');
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

  test('finishing: only a switch the operator moved is sent as a choice', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      doneNote(harness);
      await page.getByRole('button', { name: 'Finish' }).click();
      const review = page.getByRole('dialog', { name: 'Review before finishing' });
      await expect(review.getByLabel('Include picture for D1')).toBeChecked();
      await review.getByRole('button', { name: 'Confirm' }).click();
      await expect.poll(() => harness.daemon.state.confirmed?.include).toEqual({});
    } finally { await harness.stop(); }
  });

  test('finishing: switching a picture off is sent as an explicit choice', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      doneNote(harness);
      await page.getByRole('button', { name: 'Finish' }).click();
      const review = page.getByRole('dialog', { name: 'Review before finishing' });
      await review.getByLabel('Include picture for D1').uncheck();
      await review.getByRole('button', { name: 'Confirm' }).click();
      await expect.poll(() => harness.daemon.state.confirmed?.include).toEqual({ D1: false });
    } finally { await harness.stop(); }
  });

  test('finishing: when Confirm finds nothing in force the review asks to approve the design as it is', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      harness.daemon.state.unconfirmedNoChange = true;
      await page.getByRole('button', { name: 'Finish' }).click();
      const review = page.getByRole('dialog', { name: 'Review before finishing' });
      await review.getByRole('button', { name: 'Confirm' }).click();
      // No dead end and no raw error kind: the question and its answer take Confirm's place.
      await expect(review.getByText('Approve the design as it is?')).toBeVisible();
      await expect(review).not.toContainText('invalid argument');
      await review.getByRole('button', { name: 'Approve as it is' }).click();
      await expect(review).toHaveCount(0);
      expect(harness.daemon.state.confirmed.confirm_no_change).toBe(true);
    } finally { await harness.stop(); }
  });

  test('finishing: a Superseded by edit left by clicking Approve as it is still sends that click', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      doneNote(harness);
      harness.daemon.state.unconfirmedNoChange = true;
      await page.getByRole('button', { name: 'Finish' }).click();
      const review = page.getByRole('dialog', { name: 'Review before finishing' });
      await review.getByRole('button', { name: 'Confirm' }).click();
      await expect(review.getByText('Approve the design as it is?')).toBeVisible();
      const confirms = () => harness.daemon.state.requests.filter((r) => r.cmd === 'design_finish_confirm').length;
      expect(confirms()).toBe(1);
      // The edit's change event fires as the click blurs the field: the
      // review relabels in place instead of redrawing under the pointer, so
      // the click lands and, as the choice changed, asks Confirm again.
      await review.getByLabel('D1 superseded by').fill('D9');
      await review.getByRole('button', { name: 'Approve as it is' }).click();
      await expect.poll(confirms).toBe(2);
      await expect(review.getByText('Approve the design as it is?')).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('switching size hides the old picture and shows the new shape until its first frame', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      await expect(page.locator('.design-desk')).not.toHaveClass(/is-switching/);
      harness.daemon.pauseFrames();
      await page.getByRole('radio', { name: 'Phone' }).click();
      const desk = page.locator('.design-desk');
      await expect(desk).toHaveClass(/is-switching/);
      await expect(page.locator('.design-canvas')).toHaveCSS('visibility', 'hidden');
      await expect(page.locator('.design-framebox')).toHaveAttribute('data-switching', 'Switching to Phone…');
      await expect(page.getByRole('img', { name: 'Switching to Phone…' })).toBeVisible();
      const box = await page.locator('.design-framebox').boundingBox();
      expect(Math.abs(box.width / box.height - 390 / 844)).toBeLessThan(0.02);
      await page.screenshot({ path: `${EVIDENCE}/switching-to-phone.png` });
      harness.daemon.resumeFrames();
      await expect(desk).not.toHaveClass(/is-switching/);
      await expect(page.locator('.design-canvas')).toHaveCSS('visibility', 'visible');
      await expect(page.locator('.design-framebox')).not.toHaveAttribute('role', 'img');
    } finally { await harness.stop(); }
  });

  test('T-E4: a frozen old tab learns its session ended and sends nothing more', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      harness.daemon.endSession('cancelled');
      await expect(page.getByRole('heading', { name: 'Session cancelled' })).toBeVisible();
      const before = harness.daemon.state.requests.length;
      await page.keyboard.press('c');
      await page.waitForTimeout(300);
      expect(harness.daemon.state.requests.slice(before).filter((r) => r.cmd === 'design_note_add')).toEqual([]);
    } finally { await harness.stop(); }
  });
});

test.describe('end states', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('an approved design shows its own state and a way to the item', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      harness.daemon.endSession('approved');
      await expect(page.getByRole('heading', { name: 'Design approved' })).toBeVisible();
      await expect(page.locator('.design-state')).toContainText('Approved');
      await expect(page.getByRole('link', { name: 'Open the item' }).first()).toBeVisible();
      await expect(page.getByRole('button', { name: 'Back' })).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('an approved design whose item succeeded says it is built, not still building', async ({ browser, page }) => {
    const done = { id: ITEM, title: 'Make it green', state: { state: 'succeeded' }, repo: null, pipeline: 'standard', priority: 'normal', updated_at: new Date().toISOString(), created_at: new Date().toISOString(), stage: null, spent_usd: 0, dependencies: [], autonomy: { review: 'standard', proposal: 'live' } };
    const harness = await open(browser, page, { summaries: [done] });
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      harness.daemon.endSession('approved');
      await expect(page.getByRole('heading', { name: 'Design approved and built' })).toBeVisible();
      await expect(page.locator('.design-phase')).not.toContainText('building it now');
      await expect(page.locator('.design-state')).toContainText('Built');
    } finally { await harness.stop(); }
  });

  test('after Ask for changes the screen switches to the new session', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      harness.daemon.endSession('revised');
      await expect(page.getByRole('heading', { name: 'Changes requested' })).toBeVisible();
      harness.daemon.startNewSession();
      await expect(page.locator('.design-state')).toContainText('Live', { timeout: 10_000 });
      await expect(page.locator('.design-canvas')).toBeVisible();
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
      await expect(page.getByRole('heading', { name: 'Preview down' })).toBeVisible();
      await expect(page.locator('.design-phase')).toContainText('127.0.0.1:5173');
      await expect(page.getByText('Live', { exact: true })).toHaveCount(0);
      await page.screenshot({ path: `${EVIDENCE}/preview-down.png` });
      const reloads = () => harness.daemon.state.requests.filter((r) => r.cmd === 'design_reload').length;
      await page.locator('.design-phase').getByRole('button', { name: 'Retry' }).click();
      await expect.poll(reloads).toBe(1);
      harness.daemon.resumeFrames();
      await expect(page.getByRole('heading', { name: 'Preview down' })).toBeVisible(); // a frame alone does not clear it
      harness.daemon.pushSession({ degraded: false });
      await expect(page.getByRole('heading', { name: 'Preview down' })).toHaveCount(0);
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
      const scale = rect.width / 390;
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

test.describe('hardening', () => {
  test.use({ viewport: { width: 1280, height: 800 } });
  const sent = (harness, cmd) => harness.daemon.state.requests.filter((r) => r.cmd === cmd);

  test('a note is sent once however often send is pressed', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      await page.getByRole('button', { name: 'Note on the whole page' }).click();
      await page.getByLabel('What should change').fill('only once');
      await page.evaluate(() => { const f = document.querySelector('form.dcomposer'); f.requestSubmit(); f.requestSubmit(); f.requestSubmit(); });
      await expect(page.locator('#note-D1').getByText('Done', { exact: true })).toBeVisible();
      expect(sent(harness, 'design_note_add')).toHaveLength(1);
      expect(harness.daemon.state.notes).toHaveLength(1);
    } finally { await harness.stop(); }
  });

  test('[ and ] leave a note on the whole page alone', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      const canvas = page.locator('.design-canvas');
      await expect(canvas).toBeVisible();
      await page.getByRole('radio', { name: 'Comment' }).click();
      const rect = await canvas.boundingBox();
      await page.mouse.click(rect.x + rect.width / 2, rect.y + rect.height / 2);
      await expect(page.getByText('What the agent will receive')).toBeVisible();
      await page.getByRole('button', { name: 'Note on the whole page' }).click();
      await expect(page.getByText('What the agent will receive')).toBeHidden();
      await page.evaluate(() => document.activeElement?.blur());
      const before = sent(harness, 'design_resolve').length;
      await page.keyboard.press('[');
      await page.keyboard.press(']');
      await page.waitForTimeout(300);
      expect(sent(harness, 'design_resolve')).toHaveLength(before);
      await expect(page.getByText('What the agent will receive')).toBeHidden();
    } finally { await harness.stop(); }
  });

  test('a live item shows the Design chip on its queue card', async ({ browser, page }) => {
    const mk = (id, proposal) => ({ id, title: `Item ${id}`, state: { state: 'running' }, repo: null, pipeline: 'standard', priority: 'normal', updated_at: new Date().toISOString(), created_at: new Date().toISOString(), stage: 'design', spent_usd: 0, dependencies: [], autonomy: { review: 'standard', proposal } });
    const harness = await startHarness(browser, { summaries: [mk('qi_A', 'live'), mk('qi_B', 'off')] });
    try {
      await harness.install(page);
      await page.goto(`${harness.baseURL}/#/queue`);
      await expect(page.getByRole('link', { name: /Item qi_A/ }).getByText('Design', { exact: true })).toBeVisible();
      await expect(page.getByRole('link', { name: /Item qi_B/ }).getByText('Design', { exact: true })).toHaveCount(0);
    } finally { await harness.stop(); }
  });

  test('the Design it live switch puts proposal live on the add request, and off leaves it out', async ({ browser, page }) => {
    const harness = await startHarness(browser);
    try {
      await harness.install(page);
      await page.goto(`${harness.baseURL}/#/queue`);
      const queue = async (live) => {
        await page.getByRole('button', { name: 'New item', exact: true }).click();
        const sheet = page.getByRole('dialog', { name: 'New item' });
        await sheet.getByLabel('Title').fill(`Design ${live}`);
        await sheet.getByLabel('Brief').fill('Try the live design flow.');
        const toggle = sheet.getByRole('switch', { name: 'Design it live' });
        if (live) { await sheet.getByText('Design it live', { exact: true }).click(); await expect(toggle).toBeChecked(); }
        await sheet.getByRole('button', { name: 'Queue item' }).click();
        await expect(sheet).toHaveCount(0);
      };
      await queue(true);
      await queue(false);
      const adds = sent(harness, 'enqueue');
      expect(adds).toHaveLength(2);
      expect(adds[0].autonomy).toEqual({ proposal: 'live' });
      expect(adds[1].autonomy).toBeUndefined();
    } finally { await harness.stop(); }
  });
});

// The sheet and the page as a phone sees them: the notes sheet's top, and every
// element scrolled away from its start (the stage column, the sheet, the page).
const sheetAndScroll = (page) => page.evaluate(() => ({
  sheetTop: Math.round(document.querySelector('aside.design-side').getBoundingClientRect().top),
  scrolled: [...document.querySelectorAll('*')].filter((e) => e.scrollTop > 0).map((e) => `${e.tagName.toLowerCase()}.${String(e.className).split(' ')[0]}:${e.scrollTop}`),
  canvasTop: Math.round(document.querySelector('.design-canvas').getBoundingClientRect().top),
}));

// S3-phone as the validation runs it: a real phone (touch, 2x pixels, mobile
// viewport), Comment, tap a card, then Cancel in the composer.
test.describe('phone Cancel', () => {
  test.use({ viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });

  test('Cancel lowers the notes sheet to its handle and puts the page back', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      const canvas = page.locator('.design-canvas');
      await expect(canvas).toBeVisible();
      await expect(page.getByRole('radio', { name: 'Phone' })).toHaveAttribute('aria-checked', 'true');
      await expect.poll(async () => (await canvas.boundingBox())?.width ?? 0).toBeGreaterThan(300);
      const before = await sheetAndScroll(page);
      expect(before.scrolled).toEqual([]);
      await page.getByRole('navigation', { name: 'Design modes' }).getByRole('button', { name: 'Comment' }).click();
      const rect = await canvas.boundingBox();
      await page.touchscreen.tap(rect.x + rect.width * 0.3, rect.y + rect.height * 0.4);
      await expect(page.getByText('What the agent will receive')).toBeVisible();
      await expect.poll(async () => (await sheetAndScroll(page)).sheetTop).toBeLessThan(before.sheetTop - 200);
      await page.waitForTimeout(600); // the sheet has finished sliding up
      // The sheet shows the composer whole: its picture under no sticky bar, its buttons on screen.
      const head = await page.locator('.notes-head').boundingBox();
      const pic = await page.locator('.dcomposer-crop').boundingBox();
      expect(head === null || head.y + head.height <= pic.y + 1, "no sticky bar over the picture").toBe(true);
      const cancelBox = await page.getByRole("button", { name: "Cancel", exact: true }).boundingBox();
      expect(cancelBox.y + cancelBox.height, "Cancel is on screen without scrolling").toBeLessThanOrEqual(844);
      const cancel = page.getByRole('button', { name: 'Cancel', exact: true });
      await page.screenshot({ path: `${EVIDENCE}/phone-cancel-composer.png` });
      await cancel.tap();
      await expect(page.locator('.dcomposer')).toBeHidden();
      await expect.poll(() => sheetAndScroll(page)).toEqual(before);
      await page.waitForTimeout(1500);
      expect(await sheetAndScroll(page)).toEqual(before);
      await page.screenshot({ path: `${EVIDENCE}/phone-cancel-after.png` });
    } finally { await harness.stop(); }
  });
});

test.describe('phone S3 and touch', () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test('a New screen sketch fits the phone, and a tap sends the touched point', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      const canvas = page.locator('.design-canvas');
      await expect(canvas).toBeVisible();
      // The phone asks for the Phone preset; taps wait until a 390-wide frame confirms it.
      await expect(page.getByRole('radio', { name: 'Phone' })).toHaveAttribute('aria-checked', 'true');
      // Tap: the point sent is the touched one, in app pixels.
      await page.getByRole('navigation', { name: 'Design modes' }).getByRole('button', { name: 'Comment' }).click();
      const box = await harness.appPage.locator('#cta').boundingBox();
      const rect = await canvas.boundingBox();
      const scale = rect.width / 390;
      await page.touchscreen.tap(rect.x + (box.x + box.width / 2) * scale, rect.y + (box.y + box.height / 2) * scale);
      await expect(page.getByText('What the agent will receive')).toBeVisible();
      await page.screenshot({ path: `${EVIDENCE}/phone-comment.png` });
      const picked = harness.daemon.state.requests.filter((r) => r.cmd === 'design_resolve' && r.select).at(-1);
      expect(Math.abs(picked.x - (box.x + box.width / 2))).toBeLessThan(6);
      expect(Math.abs(picked.y - (box.y + box.height / 2))).toBeLessThan(6);
      await page.getByRole('button', { name: 'Cancel' }).first().click();

      await page.getByRole('button', { name: 'New screen' }).click();
      const dialog = page.getByRole('dialog', { name: 'New screen' });
      await expect(dialog).toBeVisible();
      await expect(dialog).toContainText('New screen sketch');
      await expect.poll(async () => (await dialog.boundingBox()).y).toBeLessThan(300); // the sheet has slid in
      const ink = dialog.locator('.sketch-ink');
      const ib = await ink.boundingBox();
      const db = await dialog.boundingBox();
      expect(ib.width).toBeLessThanOrEqual(db.width);
      expect(ib.y + ib.height).toBeLessThanOrEqual(844);
      await page.mouse.move(ib.x + 20, ib.y + 20);
      await page.mouse.down();
      await page.mouse.move(ib.x + 80, ib.y + 90, { steps: 4 });
      await page.mouse.up();
      await dialog.getByRole('button', { name: 'Use this picture' }).click();
      await expect.poll(() => harness.daemon.state.requests.find((r) => r.cmd === 'design_upload')?.blank).toBe(true);
      await expect(page.getByLabel('Details the agent will receive')).toHaveValue(/New screen sketch/);
      await expect(page.getByLabel('Details the agent will receive')).not.toHaveValue(/Region the operator drew/);
    } finally { await harness.stop(); }
  });
});
