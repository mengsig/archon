// T-E1..T-E6 of the live design sessions plan, against a stand-in host
// (support/design-harness.js): the real web app and the fixture app in
// fixtures/design-app, with a scripted daemon. A real archond run of the same
// flows belongs to the daemon tasks' own e2e.
import { test as base, expect } from '@playwright/test';
import { mkdirSync } from 'node:fs';
import { startHarness, seedNote, ITEM } from './support/design-harness.js';
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

const presetRadio = (page, name) => page.getByRole('radiogroup', { name: /size/i }).getByRole('radio', { name });
const viewportAsks = (harness) => harness.daemon.state.requests.filter((r) => r.cmd === 'design_viewport').map((r) => r.preset);

test.describe('desktop', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('T-E1: comment, send, watch it turn green, compare, undo, finish', async ({ browser, page }) => {
    const gate = { id: 'q_GATE', item: ITEM, number: 1, text: 'Approve the design?', options: ['approve', 'revise', 'reject'], asked_at: new Date().toISOString(), answer: null };
    const unrelated = { ...gate, id: 'q_OTHER', number: 2, text: 'Unrelated question' };
    const harness = await open(browser, page, { questions: [unrelated, gate] });
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

  test('finishing: an asset-only picture counts, can be retried and can be excluded', async ({ browser, page }) => {
    const harness = await startHarness(browser, { notes: [seedNote({ number: 1 })] });
    try {
      harness.daemon.state.assetOnlyFinish = true;
      harness.daemon.state.refuseFinalAsset = true;
      harness.daemon.state.imagesUsed = 119;
      await harness.install(page);
      await page.goto(`${harness.baseURL}/#/design/${ITEM}`);
      await page.getByRole('button', { name: 'Finish' }).click();
      const review = page.getByRole('dialog', { name: 'Review before finishing' });
      const include = review.getByRole('switch', { name: 'Include picture for D1' });
      await expect(include).toBeEnabled();
      await expect(include).toBeChecked();
      await expect(review.getByText('120 of 128 pictures are used', { exact: false })).toBeVisible();
      await expect(review.getByRole('alert')).toHaveText('Try the picture again.');
      harness.daemon.state.refuseFinalAsset = false;
      await review.getByRole('button', { name: 'Retry final picture for D1' }).click();
      await expect(review.getByRole('img', { name: 'Final picture for D1' })).toBeVisible();
      const loads = () => sentOf(harness, 'design_asset').filter((r) => r.id === 'da_f');
      expect(loads()).toHaveLength(2);
      expect(loads().every((request) => request.item === ITEM)).toBe(true);
      await include.uncheck();
      await expect(review.getByText('120 of 128 pictures are used', { exact: false })).toHaveCount(0);
      await review.getByRole('button', { name: 'Confirm' }).click();
      await expect.poll(() => harness.daemon.state.confirmed?.include).toEqual({ D1: false });
      expect(loads()).toHaveLength(2);
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

  test('finishing: choose a replacement from current notes and confirm', async ({ browser, page }) => {
    const harness = await open(browser, page, { notes: [seedNote({ number: 2, text: 'replace green' })] });
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
      const replacement = review.getByLabel('D1 superseded by');
      await expect(replacement.locator('option')).toHaveText(['Not superseded', 'D2 · replace green']);
      await expect(review.getByLabel('D2 superseded by').locator('option')).toHaveText(['Not superseded']);
      await replacement.selectOption('D2');
      await review.getByRole('button', { name: 'Confirm', exact: true }).click();
      await expect.poll(confirms).toBe(2);
      expect(harness.daemon.state.requests.filter((r) => r.cmd === 'design_finish_confirm').at(-1).superseded.D1).toBe('D2');
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

  test('a size chosen over a live picture is asked for once', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(presetRadio(page, 'Desktop')).toHaveAttribute('aria-checked', 'true');
      await presetRadio(page, 'Tablet').click();
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-checked', 'true');
      await page.waitForTimeout(300);
      expect(viewportAsks(harness)).toEqual(['tablet']);
    } finally { await harness.stop(); }
  });

  test('a refused size goes back to the last one archond applied', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await presetRadio(page, 'Tablet').click();
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-checked', 'true');
      harness.daemon.refuseViewport('the app has a dialog open; answer it first');
      await presetRadio(page, 'Phone').click();
      await expect(page.getByText(/dialog open/i)).toBeVisible();
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-checked', 'true');
      await expect(page.locator('.design-desk')).not.toHaveClass(/is-switching/);
      expect(viewportAsks(harness)).toEqual(['tablet', 'phone']);
    } finally { await harness.stop(); }
  });

  test('a size refused while the app shows a dialog leaves nothing switching', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(presetRadio(page, 'Desktop')).toHaveAttribute('aria-checked', 'true');
      harness.daemon.refuseViewport('the app has a dialog open; answer it first');
      await presetRadio(page, 'Tablet').click();
      await expect(page.getByText(/dialog open/i)).toBeVisible();
      await expect(presetRadio(page, 'Desktop')).toHaveAttribute('aria-checked', 'true');
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-busy', 'false');
      await expect(page.locator('.design-desk')).not.toHaveClass(/is-switching/);
      await expect(page.locator('.design-canvas')).toHaveCSS('visibility', 'visible');
      await presetRadio(page, 'Tablet').click();
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-checked', 'true');
      expect(viewportAsks(harness)).toEqual(['tablet', 'tablet']);
    } finally { await harness.stop(); }
  });

  test('sizes chosen while one is switching: one request at a time, and the latest wins', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(presetRadio(page, 'Desktop')).toHaveAttribute('aria-checked', 'true');
      const release = harness.daemon.holdViewport();
      await presetRadio(page, 'Tablet').click();
      await expect.poll(() => viewportAsks(harness)).toEqual(['tablet']);
      await presetRadio(page, 'Desktop').click();
      await presetRadio(page, 'Phone').click();
      await page.waitForTimeout(300);
      expect(viewportAsks(harness), 'nothing more while Tablet is unanswered').toEqual(['tablet']);
      release();
      await expect(presetRadio(page, 'Phone')).toHaveAttribute('aria-checked', 'true');
      await page.waitForTimeout(300);
      expect(viewportAsks(harness), 'Desktop was replaced before it was sent').toEqual(['tablet', 'phone']);
    } finally { await harness.stop(); }
  });

  test('a view asked for before a resize does not undo it', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      await expect(presetRadio(page, 'Desktop')).toHaveAttribute('aria-checked', 'true');
      // A refresh goes out while the session is still on Desktop, and its answer comes late.
      harness.daemon.state.notes.unshift(seedNote({ id: 'D7', text: 'the late view arrived', before_asset: null, after_asset: null }));
      harness.daemon.state.version += 1;
      const late = harness.daemon.holdView();
      const gets = () => harness.daemon.state.requests.filter((r) => r.cmd === 'design_get').length;
      const before = gets();
      harness.daemon.pushSession({});
      await expect.poll(gets).toBe(before + 1);
      await presetRadio(page, 'Tablet').click();
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-checked', 'true');
      late.release();
      await late.delivered;
      await expect(page.getByText('the late view arrived')).toBeVisible();
      await page.waitForTimeout(300);
      await expect(presetRadio(page, 'Tablet')).toHaveAttribute('aria-checked', 'true');
      expect(viewportAsks(harness)).toEqual(['tablet']);
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

  test('offline draft across a replacement session', async ({ browser, page }) => {
    const harness = await open(browser, page);
    const { daemon } = harness;
    const storageKey = `archon.design.outbox.${ITEM}`;
    try {
      await expect(page.locator('.design-canvas')).toBeVisible();
      await page.getByRole('button', { name: 'Note on the whole page' }).click();
      const draft = 'Move the checkout button below the total';
      await page.getByLabel('What should change').fill(draft);
      daemon.state.onNextNoteAdd = (ws) => {
        ws.close();
        daemon.startNewSession();
      };
      await page.getByLabel('What should change').press('Enter');
      const adds = () => daemon.state.requests.filter((r) => r.cmd === 'design_note_add');
      await expect.poll(() => daemon.state.requests.some((r) => r.cmd === 'subscribe_design' && r.session === 'ds_SECOND')).toBe(true);
      await expect(page.locator('.design-canvas')).toBeVisible();
      expect(adds().map((r) => r.session)).toEqual(['ds_FIXTURE']);
      expect(daemon.state.notes).toEqual([]);
      const recover = page.getByRole('button', { name: 'Unsent drafts' });
      await expect(recover).toBeVisible();
      await page.evaluate((key) => {
        const saved = JSON.parse(localStorage.getItem(key));
        saved.push({ client_key: 'web-legacy', text: 'Keep the totals visible', details_text: 'The old sidebar had a summary.' });
        localStorage.setItem(key, JSON.stringify(saved));
      }, storageKey);
      await page.reload();
      await expect(page.locator('.design-canvas')).toBeVisible();
      await recover.focus();
      await recover.press('Enter');
      const dialog = page.getByRole('dialog', { name: 'Unsent drafts' });
      const cards = dialog.getByRole('listitem');
      await expect(cards).toHaveCount(2);
      const [earlier, legacy] = await cards.all();
      await expect(earlier).toContainText('This draft belongs to an earlier design session.');
      const words = earlier.getByRole('textbox', { name: 'Note text' });
      await expect(words).toHaveValue(draft);
      await words.press('ControlOrMeta+a');
      expect(await words.evaluate((el) => el.value.slice(el.selectionStart, el.selectionEnd))).toBe(draft);
      await expect(legacy).toContainText('The original design session is unknown.');
      await expect(legacy.getByRole('textbox', { name: 'Note text' })).toHaveValue('Keep the totals visible');
      await expect(legacy.getByRole('textbox', { name: 'Note details' })).toHaveValue('The old sidebar had a summary.');
      await page.screenshot({ path: `${EVIDENCE}/offline-replacement-after.png`, animations: 'disabled' });
      await page.keyboard.press('Escape');
      await expect(recover).toBeFocused();
      expect(adds().map((r) => r.session)).toEqual(['ds_FIXTURE']);
    } finally { await harness.stop(); }
  });

  test('drafts are recoverable when the initial session read fails and after the session ends', async ({ browser, page }) => {
    const harness = await startHarness(browser);
    const { daemon } = harness;
    const draft = { session: 'ds_FIXTURE', client_key: 'web-recovery', text: 'Move the checkout button below the total' };
    try {
      daemon.state.designGetError = { code: 'unavailable', message: 'The design session is temporarily unavailable.' };
      await page.addInitScript(({ key, draft }) => localStorage.setItem(key, JSON.stringify([draft])),
        { key: `archon.design.outbox.${ITEM}`, draft });
      await harness.install(page);
      await page.goto(`${harness.baseURL}/#/design/${ITEM}`);
      await expect.poll(() => daemon.state.requests.some((r) => r.cmd === 'design_get')).toBe(true);

      const recover = page.getByRole('button', { name: 'Unsent drafts' });
      const dialog = page.getByRole('dialog', { name: 'Unsent drafts' });
      await recover.click();
      await expect(dialog).toContainText('Waiting to verify the original design session.');
      await expect(dialog.getByRole('textbox', { name: 'Note text' })).toHaveValue(draft.text);
      await dialog.getByRole('button', { name: 'Close' }).click();
      await expect(dialog).toBeHidden();

      daemon.state.designGetError = null;
      daemon.endSession('revised');
      await page.reload();
      await expect(page.getByRole('heading', { name: 'Changes requested' })).toBeVisible();
      await recover.click();
      await expect(dialog).toContainText('The original design session has ended.');
      await expect(dialog.getByRole('textbox', { name: 'Note text' })).toHaveValue(draft.text);
      expect(daemon.state.requests.filter((r) => r.cmd === 'design_note_add')).toEqual([]);
    } finally { await harness.stop(); }
  });

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

  test('reconnecting waits for a fresh picture and clears the previous canvas', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      const canvas = page.locator('.design-canvas');
      const alpha = () => canvas.evaluate((c) => c.getContext('2d').getImageData(0, 0, 1, 1).data[3]);
      await expect(canvas).toBeVisible();
      expect(await alpha()).toBe(255);
      harness.daemon.pauseFrames();
      harness.daemon.dropSockets();
      await expect(page.getByRole('heading', { name: 'Starting your app…' })).toBeVisible();
      await expect(canvas).toBeHidden();
      expect(await alpha()).toBe(0);
      harness.daemon.resumeFrames();
      await expect(canvas).toBeVisible();
      expect(await alpha()).toBe(255);
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
        await expect(sheet.getByLabel('Title')).toBeFocused();
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

test.describe('scrolling', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('a fast wheel on a slow preview never floods it, loses no distance and shows no red error', async ({ browser, page }) => {
    const harness = await open(browser, page);
    try {
      const canvas = page.locator('.design-canvas');
      await expect(canvas).toBeVisible();
      await expect(page.getByText('Live', { exact: true }).first()).toBeVisible();
      const state = harness.daemon.state;
      state.inputDelayMs = 800;
      const rect = await canvas.boundingBox();
      const scale = rect.width / (await canvas.evaluate((c) => c.width));
      await page.mouse.move(rect.x + rect.width / 2, rect.y + rect.height / 2);
      for (let i = 0; i < 60; i += 1) await page.mouse.wheel(0, 50);
      const inputs = () => state.requests.filter((r) => r.cmd === 'design_input' && !r.refused);
      const scrolled = () => inputs().flatMap((r) => r.events).filter((e) => e.type === 'wheel').reduce((sum, e) => sum + e.dy, 0);
      await expect(page.locator('.design-busy')).toHaveText('Catching up with the preview…');
      // Each tick is rounded to 0.1 app px; the rest of the distance arrives.
      await expect.poll(scrolled, { timeout: 15_000 }).toBeGreaterThan(60 * 50 / scale - 60 * 0.05);
      expect(scrolled()).toBeLessThan(60 * 50 / scale + 60 * 0.05);
      expect(state.maxInputsInFlight).toBe(1);
      expect(inputs().length).toBeLessThan(30);
      await expect(page.locator('.design-busy')).toBeHidden();

      // archond's own in-flight refusal is backpressure: one quiet line, never a red toast,
      // and the refused input goes again until it is taken.
      const before = scrolled();
      state.inputDelayMs = 0;
      state.refuseInputs = 3;
      for (let i = 0; i < 3; i += 1) { await page.mouse.wheel(0, 50); await page.waitForTimeout(40); }
      await expect(page.locator('.design-busy')).toBeVisible();
      await expect(page.locator('.design-busy')).toHaveCount(1);
      await page.screenshot({ path: `${EVIDENCE}/design-scroll-backpressure.png` });
      await expect.poll(() => scrolled() - before).toBeGreaterThan(3 * 50 / scale - 3 * 0.05);
      expect(state.refuseInputs).toBe(0);
      await expect(page.locator('.design-busy')).toBeHidden();
      await expect(page.locator('.toast-red')).toHaveCount(0);
    } finally { await harness.stop(); }
  });
});

// ---- design iterations: follow-ups, references, the tree, viewing and continuing ----

// D3 → D7 → D9 (D9 refers to D5); D5 with D6 set aside (undone by D11) and D8 working; D10 queued on the page.
function iterationNotes() {
  const at = (n, f = {}) => seedNote({ number: n, ...f });
  return [
    at(3, { text: 'Make the header taller', commit: '9ab02f1aaaa', thread: [{ at: '2026-01-01T00:00:00Z', from: 'agent', text: 'Header height 56 to 72px.' }] }),
    at(5, { text: 'Green primary buttons', commit: '51aa7e0bbbb' }),
    at(6, { text: 'rounder corners', follows: 'D5' }),
    at(7, { text: 'bigger title', follows: 'D3', commit: '4c1e9a2cccc' }),
    at(8, { text: 'outline style', follows: 'D5', status: 'working', commit: null, after_asset: null }),
    at(9, { text: 'green, like D5', follows: 'D7', refs: ['D5'], commit: 'e71d0c3dddd' }),
    at(10, { kind: 'page', text: 'Combine D3 and D5 on Settings', refs: ['D3', 'D5'], status: 'queued', commit: null, before_asset: null, after_asset: null }),
    at(11, { kind: 'revert', reverts: 'D6', text: '', commit: 'rev00001' }),
  ];
}
const sentOf = (harness, cmd) => harness.daemon.state.requests.filter((r) => r.cmd === cmd);

test.describe('iterations (desktop)', () => {
  test.use({ viewport: { width: 1280, height: 720 }, colorScheme: 'light' });

  test('follow-up loop: send a follow-up, the agent replies in the same thread', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: [seedNote({ number: 1, text: 'make it green' })] });
    try {
      const chain = page.locator('#chain-D1');
      await expect(chain).toBeVisible();
      await chain.getByRole('button', { name: 'Follow up' }).click();
      const box = chain.getByLabel('Follow-up on D1');
      await expect(chain.getByText('Follow up on D1 (newest)')).toBeVisible();
      await box.fill('and make it bigger');
      await chain.getByRole('button', { name: 'Send follow-up' }).click();
      await expect.poll(() => sentOf(harness, 'design_note_add').length).toBe(1);
      const sent = sentOf(harness, 'design_note_add')[0];
      expect(sent).toMatchObject({ text: 'and make it bigger', follows: 'D1' });
      expect(sent.client_key).toMatch(/^web-/);
      await expect(chain.getByText('2 versions')).toBeVisible();
      await expect(chain.locator('#note-D2').getByText('Made the button green.')).toBeVisible();
      await expect(chain.locator('#note-D2').getByText('Follow-up on D1')).toBeVisible();
      await expect(chain.getByLabel('Follow-up on D1')).toHaveCount(0); // the box closed after sending
    } finally { await harness.stop(); }
  });

  test('references: the picker, @D autocomplete, chips, links, and the request carries refs', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      const chain = page.locator('#chain-D3');
      await chain.getByRole('button', { name: 'Follow up' }).click();
      const box = chain.getByLabel(/Follow-up on D9/);
      await box.fill('slightly darker');
      await chain.getByRole('button', { name: '+ Reference a change' }).click();
      const picker = chain.getByRole('list', { name: 'Earlier notes to reference' });
      await expect(picker.getByRole('button')).toHaveCount(8 - 1); // every note but the revert
      await picker.getByRole('button', { name: /^D5 / }).click();
      await expect(chain.getByRole('button', { name: 'Remove reference D5' })).toHaveText('D5 ×');
      await box.fill('slightly darker, like @D');
      const mentions = chain.getByRole('listbox', { name: 'Notes matching what you typed' });
      await expect(mentions.getByRole('option').first()).toContainText('D10');
      await mentions.getByRole('option', { name: /^D3/ }).click();
      await expect(box).toHaveValue('slightly darker, like D3 ');
      await expect(chain.getByRole('button', { name: 'Remove reference D3' })).toBeVisible();
      await chain.getByRole('button', { name: 'Remove reference D3' }).click();
      await expect(chain.getByRole('button', { name: 'Remove reference D3' })).toHaveCount(0);
      await box.fill('slightly darker, like @D3');
      await expect(mentions).toBeVisible();
      await page.keyboard.press('Enter'); // the open list takes Enter: nothing is sent
      expect(sentOf(harness, 'design_note_add')).toHaveLength(0);
      await expect(chain.getByRole('button', { name: 'Remove reference D3' })).toBeVisible();
      await chain.getByRole('button', { name: 'Send follow-up' }).click();
      await expect.poll(() => sentOf(harness, 'design_note_add').length).toBe(1);
      expect(sentOf(harness, 'design_note_add')[0]).toMatchObject({ follows: 'D9', refs: ['D5', 'D3'] });
      // After sending: D3, typed, is a link in the words; D5, only picked, has its own link.
      const sentCard = chain.locator('.note').filter({ hasText: 'slightly darker, like D3' });
      await expect(sentCard.locator('.note-text').getByRole('button', { name: 'Show D3 in the tree' })).toBeVisible();
      await expect(sentCard.locator('.note-refs').getByRole('button', { name: 'Show D5 in the tree' })).toBeVisible();
      await expect(sentCard.locator('.note-refs').getByRole('button', { name: 'Show D3 in the tree' })).toHaveCount(0);
      // A note whose words already name every reference adds no line under them.
      await expect(page.locator('#note-D9 .note-refs')).toHaveCount(0);
      await expect(page.locator('#note-D9')).not.toContainText('null');
      // D9's own text links D5; a link selects that node in the tree.
      await page.locator('#note-D9').getByRole('button', { name: 'Show D5 in the tree' }).click();
      await expect(page.getByRole('tab', { name: 'Tree' })).toHaveAttribute('aria-selected', 'true');
      await expect(page.locator('.tree-node[data-note="D5"]')).toHaveClass(/is-viewing/);
      await expect.poll(() => harness.daemon.state.notes.some((n) => n.follows === 'D9' && n.status === 'done')).toBe(true);
    } finally { await harness.stop(); }
  });

  test('a sixth reference is refused in plain words', async ({ browser, page }) => {
    const notes = [1, 2, 3, 4, 5, 6, 7].map((n) => seedNote({ number: n, text: `note ${n}` }));
    const harness = await open(browser, page, { iterations: true, notes });
    try {
      const chain = page.locator('#chain-D7');
      await chain.getByRole('button', { name: 'Follow up' }).click();
      await chain.getByRole('button', { name: '+ Reference a change' }).click();
      for (const id of ['D1', 'D2', 'D3', 'D4', 'D5']) {
        await chain.getByRole('list', { name: 'Earlier notes to reference' }).getByRole('button', { name: new RegExp(`^${id} `) }).click();
        if (id !== 'D5') await chain.getByRole('button', { name: '+ Reference a change' }).click();
      }
      await chain.getByRole('button', { name: '+ Reference a change' }).click();
      await chain.getByRole('list', { name: 'Earlier notes to reference' }).getByRole('button', { name: /^D6 / }).click();
      await expect(chain.getByText('A note can refer to at most 5 earlier notes.')).toBeVisible();
      await expect(chain.locator('.ref-chip')).toHaveCount(5);
    } finally { await harness.stop(); }
  });

  test('Notes and Tree tabs are named and reachable with the arrow keys', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true });
    try {
      const notesTab = page.getByRole('tab', { name: 'Notes' });
      const treeTab = page.getByRole('tab', { name: 'Tree' });
      await notesTab.focus();
      await notesTab.press('ArrowRight');
      await expect(treeTab).toBeFocused();
      await expect(treeTab).toHaveAttribute('aria-selected', 'true');
      await expect(page.getByRole('tabpanel', { name: 'Tree' })).toBeVisible();
      await treeTab.press('ArrowLeft');
      await expect(notesTab).toBeFocused();
      await expect(page.getByRole('tabpanel', { name: 'Notes' })).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('tree: groups, nodes, branches, set-aside and current', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      await page.getByRole('tab', { name: 'Tree' }).click();
      await expect(page.locator('.tree-group-title')).toHaveText(['D10 · PAGE', 'D3 · HEADER', 'D5 · BUTTONS']);
      const node = (id) => page.locator(`.tree-node[data-note="${id}"]`);
      await expect(node('D3')).toHaveClass(/depth-0/);
      await expect(node('D7')).toHaveClass(/depth-1/);
      await expect(node('D9')).toHaveClass(/depth-2/);
      await expect(node('D9').getByText('current')).toBeVisible();
      await expect(node('D9')).toContainText('↳ follow-up: green, like D5 · e71d0c3');
      await expect(node('D3')).toContainText('9ab02f1');
      // A branch: D6 and D8 both continue D5.
      await expect(node('D6')).toHaveClass(/depth-1/);
      await expect(node('D8')).toHaveClass(/depth-1/);
      await expect(node('D6')).toHaveClass(/is-aside/);
      await expect(node('D6').getByText('Set aside')).toBeVisible();
      await expect(node('D8')).not.toHaveClass(/is-aside/);
      await expect(node('D8').getByText('Working')).toBeVisible();
      await expect(node('D10').getByText('Queued')).toBeVisible();
      await expect(node('D3').locator('.thumb-img')).toBeVisible();
      await expect(node('D10').locator('.thumb-img')).toHaveCount(0);
      // References are links beside the node; one selects its node.
      await expect(node('D10').locator('.tree-node-refs').getByRole('button')).toHaveText(['D3', 'D5']);
      await node('D9').getByRole('button', { name: 'Show D5 in the tree' }).click();
      await expect(node('D5')).toHaveClass(/is-viewing/);
    } finally { await harness.stop(); }
  });

  test('viewing an older version: the bar, Compare with…, Back to current; the live preview is untouched', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D7"] .tree-node-main').click();
      const bar = page.locator('.viewer-bar');
      await expect(bar).toContainText('Viewing D7 · version 2 of 3');
      await expect(bar).toContainText('pictures at commit 4c1e9a2');
      await expect(bar).toContainText('The live preview still shows the current version.');
      await expect(page.locator('.design-viewer .cmp-img').first()).toBeVisible();
      await expect(page.locator('.design-canvas')).toBeHidden();
      await page.getByLabel('Compare with…').selectOption({ label: 'D9·v3 (current)' });
      await expect(page.locator('.viewer-side .cmp-img')).toHaveCount(2);
      await expect(page.locator('.viewer-side')).toContainText('After D7·v2');
      await expect(page.locator('.viewer-side')).toContainText('After D9·v3');
      await page.getByLabel('Compare with…').selectOption('');
      await expect(page.locator('.viewer-side')).toHaveCount(0);
      await expect(page.locator('.design-viewer .cmp-img')).toHaveCount(2); // this version's own before and after
      await page.getByRole('button', { name: 'Back to current' }).click();
      await expect(page.locator('.design-viewer')).toBeHidden();
      await expect(page.locator('.design-canvas')).toBeVisible();
      // Viewing sent nothing that changes the preview or the code.
      const changing = ['design_viewport', 'design_reload', 'design_note_undo', 'design_note_continue_from', 'design_note_add'];
      expect(harness.daemon.state.requests.filter((r) => changing.includes(r.cmd))).toEqual([]);
      // The newest version has nothing to continue from.
      await page.locator('.tree-node[data-note="D9"] .tree-node-main').click();
      await expect(page.locator('.design-viewer').getByRole('button', { name: 'Continue from this version' })).toBeHidden();
      await expect(page.locator('.design-viewer').getByRole('button', { name: 'Follow up on this version' })).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('continue from an older version: confirm, reverts queue, the follow-up box opens on it', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D3"] .tree-node-main').click();
      await page.locator('.design-viewer').getByRole('button', { name: 'Continue from this version' }).click();
      const dialog = page.getByRole('dialog', { name: 'Continue from D3?' });
      await expect(dialog).toContainText('D7, D9');
      await dialog.getByRole('button', { name: 'Keep as it is' }).click();
      await expect(dialog).toHaveCount(0);
      expect(sentOf(harness, 'design_note_continue_from')).toHaveLength(0);
      await page.locator('.design-viewer').getByRole('button', { name: 'Continue from this version' }).click();
      await page.getByRole('dialog', { name: 'Continue from D3?' }).getByRole('button', { name: 'Continue from this version' }).click();
      await expect.poll(() => sentOf(harness, 'design_note_continue_from').length).toBe(1);
      expect(sentOf(harness, 'design_note_continue_from')[0]).toMatchObject({ id: 'D3', text: null, refs: [] });
      // The follow-up box is open on D3, in the Notes tab; the newer versions were reverted and set aside.
      await expect(page.getByRole('tab', { name: 'Notes' })).toHaveAttribute('aria-selected', 'true');
      await expect(page.getByLabel('Follow-up on D3')).toBeVisible();
      await expect.poll(() => harness.daemon.state.notes.filter((n) => n.kind === 'revert' && n.reverts && n.status === 'done').map((n) => n.reverts).sort()).toEqual(['D6', 'D7', 'D9']);
      await expect(page.locator('#note-D9').getByText('Set aside')).toBeVisible();
      await page.getByLabel('Follow-up on D3').fill('make it blue');
      await page.getByRole('button', { name: 'Send follow-up' }).click();
      await expect.poll(() => sentOf(harness, 'design_note_add').length).toBe(1);
      expect(sentOf(harness, 'design_note_add')[0]).toMatchObject({ follows: 'D3', text: 'make it blue' });
      expect(sentOf(harness, 'design_note_continue_from')).toHaveLength(1);
      await expect.poll(() => harness.daemon.state.notes.some((n) => n.follows === 'D3' && n.status === 'done')).toBe(true);
    } finally { await harness.stop(); }
  });

  test('follow up on an older version: confirm, then design_note_continue_from carries the text', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D7"] .tree-node-main').click();
      await page.locator('.design-viewer').getByRole('button', { name: 'Follow up on this version' }).click();
      await expect(page.getByText('Sending this first undoes D9')).toBeVisible();
      await page.getByLabel('Follow-up on D7').fill('the title in blue');
      await page.getByLabel('Follow-up on D7').press('Enter');
      const dialog = page.getByRole('dialog', { name: 'Follow up on D7?' });
      await dialog.getByRole('button', { name: 'Keep as it is' }).click();
      await expect(dialog).toHaveCount(0);
      expect(sentOf(harness, 'design_note_continue_from')).toHaveLength(0);
      await expect(page.getByLabel('Follow-up on D7')).toHaveValue('the title in blue'); // kept for another try
      await page.getByRole('button', { name: 'Send follow-up' }).click();
      await page.getByRole('dialog', { name: 'Follow up on D7?' }).getByRole('button', { name: 'Undo and follow up' }).click();
      await expect.poll(() => sentOf(harness, 'design_note_continue_from').length).toBe(1);
      expect(sentOf(harness, 'design_note_continue_from')[0]).toMatchObject({ id: 'D7', text: 'the title in blue', refs: [] });
      expect(sentOf(harness, 'design_note_add')).toHaveLength(0);
      await expect(page.locator('#note-D12 .note-text, #note-D13 .note-text, #note-D14 .note-text').filter({ hasText: 'the title in blue' })).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('continuing from a version refused or in conflict shows the plain message', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      harness.daemon.state.refuseContinue = 'invalid argument: this change has a version that cannot be undone';
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D3"] .tree-node-main').click();
      await page.locator('.design-viewer').getByRole('button', { name: 'Continue from this version' }).click();
      await page.getByRole('dialog').getByRole('button', { name: 'Continue from this version' }).click();
      await expect(page.getByText('This change has a version that cannot be undone.')).toBeVisible();
      harness.daemon.state.refuseContinue = null;
      harness.daemon.state.conflictOn = 'D9';
      await page.locator('.design-viewer').getByRole('button', { name: 'Follow up on this version' }).click();
      await page.getByLabel('Follow-up on D3').fill('again');
      await page.getByRole('button', { name: 'Send follow-up' }).click();
      await page.getByRole('dialog').getByRole('button', { name: 'Undo and follow up' }).click();
      await expect(page.getByText(/Can’t undo automatically/)).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('finish review shows one compact history line per chain card', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes().filter((n) => n.id !== 'D10') });
    try {
      await page.getByRole('button', { name: 'Finish' }).click();
      const review = page.getByRole('dialog', { name: 'Review before finishing' });
      await expect(review.locator('.finish-card')).toHaveCount(2); // D9 (chain D3) and D5 (D6 is set aside, D8 is not done)
      await expect(review.getByText('History: D3 Make the header taller › D7 bigger title')).toBeVisible();
      // Each change of the chain, not only its final picture: before and after for D3, D7 and D9.
      const changes = review.getByRole('list', { name: 'Changes in D9' });
      await expect(changes.getByRole('listitem')).toHaveCount(3);
      await expect(changes.locator('.finish-change-title')).toHaveText(['D3 Make the header taller', 'D7 bigger title', 'D9 green, like D5']);
      await expect(changes.getByRole('img')).toHaveCount(6);
      await expect(changes.getByRole('img', { name: 'After picture of D7' })).toBeVisible();
      await expect(changes.locator('figcaption')).toHaveText(['Your sketch', 'After', 'Your sketch', 'After', 'Your sketch', 'After']);
      await expect(review.getByRole('list', { name: 'Changes in D5' }).getByRole('listitem')).toHaveCount(1);
      await expect(review.getByRole('list', { name: 'Changes in D5' }).locator('.finish-change-title')).toHaveCount(0); // the card's head names it
      await expect(review.getByRole('list', { name: 'Changes in D5' }).getByRole('img')).toHaveCount(2);
    } finally { await harness.stop(); }
  });

  test('the continue dialog names every version it reverts and those already set aside', async ({ browser, page }) => {
    const notes = [
      seedNote({ number: 1, text: 'Make the header taller' }),
      seedNote({ number: 2, follows: 'D1', text: 'bigger title' }),
      seedNote({ number: 3, follows: 'D2', text: 'rounder corners' }),
      seedNote({ number: 4, follows: 'D2', text: 'outline style' }),
      seedNote({ number: 5, kind: 'revert', reverts: 'D3', text: '', commit: 'rev00001' }),
    ];
    const harness = await open(browser, page, { iterations: true, notes });
    try {
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D1"] .tree-node-main').click();
      await page.locator('.design-viewer').getByRole('button', { name: 'Continue from this version' }).click();
      const dialog = page.getByRole('dialog', { name: 'Continue from D1?' });
      await expect(dialog).toContainText('Archon reverses D2, D4 with new commits (nothing is erased) and marks them Set aside. D3 is already set aside and stays so. Then you can follow up on D1.');
      await dialog.getByRole('button', { name: 'Continue from this version' }).click();
      await expect.poll(() => harness.daemon.state.notes.filter((n) => n.kind === 'revert' && n.status === 'done').map((n) => n.reverts).sort()).toEqual(['D2', 'D3', 'D4']);
    } finally { await harness.stop(); }
  });

  test('an older host: no tabs, no follow-up, no references', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: false, notes: [seedNote({ number: 1, text: 'make it green' })] });
    try {
      await expect(page.locator('#note-D1')).toBeVisible();
      await expect(page.getByRole('tab')).toHaveCount(0);
      await expect(page.getByRole('button', { name: 'Follow up' })).toHaveCount(0);
      await expect(page.locator('#chain-D1')).toHaveCount(0);
    } finally { await harness.stop(); }
  });
});

test.describe('iterations (phone)', () => {
  test.use({ viewport: { width: 390, height: 844 }, colorScheme: 'light' });

  test('the sheet: Tree tab, node detail inline, open compare, continue from here', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      await page.getByRole('navigation', { name: 'Design modes' }).getByRole('button', { name: 'Notes', exact: true }).click();
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D7"] .tree-node-main').click();
      const detail = page.getByRole('group', { name: 'Version D7' });
      // The same words as the desktop viewer: its line, its captions and its buttons.
      await expect(detail).toContainText('Viewing D7 · version 2 of 3 (pictures at commit 4c1e9a2). The live preview still shows the current version.');
      await expect(detail.getByText('Your sketch', { exact: true })).toBeVisible(); // a note with no target: its before picture is the sketch
      await expect(detail.getByText('After', { exact: true })).toBeVisible();
      await expect(detail.getByRole('button')).toHaveText(['Back to current', 'Compare', 'Follow up on this version', 'Continue from this version']);
      await expect(page.locator('.design-viewer')).toBeHidden();
      await detail.getByRole('button', { name: 'Compare', exact: true }).click();
      await expect(page.getByRole('dialog', { name: 'D7: before and after' })).toBeVisible();
      await expect(page.getByRole('dialog', { name: 'D7: before and after' }).getByText('Your sketch', { exact: true })).toBeVisible();
      await page.getByRole('dialog', { name: 'D7: before and after' }).getByRole('button', { name: 'Close' }).click();
      await expect(page.getByRole('dialog')).toHaveCount(0);
      await detail.getByRole('button', { name: 'Back to current' }).click();
      await expect(page.getByRole('group', { name: 'Version D7' })).toHaveCount(0);
      await page.locator('.tree-node[data-note="D3"] .tree-node-main').click();
      await page.getByRole('group', { name: 'Version D3' }).getByRole('button', { name: 'Continue from this version' }).click();
      await page.getByRole('dialog', { name: 'Continue from D3?' }).getByRole('button', { name: 'Continue from this version' }).click();
      await expect.poll(() => sentOf(harness, 'design_note_continue_from').length).toBe(1);
      await expect(page.getByLabel('Follow-up on D3')).toBeVisible();
    } finally { await harness.stop(); }
  });

  test('a deep tree fits the phone: no sideways scroll, nodes keep their width', async ({ browser, page }) => {
    const notes = [seedNote({ number: 1, text: 'Make the header taller' })];
    for (let n = 2; n <= 9; n += 1) notes.push(seedNote({ number: n, follows: `D${n - 1}`, text: n === 8 ? 'https://example.com/a-very-long-address-with-no-spaces-to-break-the-line-anywhere' : `step ${n}`, refs: n === 9 ? ['D1', 'D2', 'D3', 'D4', 'D5'] : [] }));
    const harness = await open(browser, page, { iterations: true, notes });
    try {
      await page.getByRole('navigation', { name: 'Design modes' }).getByRole('button', { name: 'Notes', exact: true }).click();
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D9"] .tree-node-main').click();
      await expect(page.getByRole('group', { name: 'Version D9' })).toBeVisible();
      const sizes = await page.evaluate(() => {
        const side = document.querySelector('.design-side');
        return { scroll: side.scrollWidth, client: side.clientWidth, page: document.documentElement.scrollWidth, deepest: document.querySelector('.tree-node[data-note="D9"] .tree-node-main').getBoundingClientRect().width };
      });
      expect(sizes.scroll).toBeLessThanOrEqual(sizes.client);
      expect(sizes.page).toBeLessThanOrEqual(390);
      expect(sizes.deepest).toBeGreaterThanOrEqual(280);
    } finally { await harness.stop(); }
  });
});

// The three states of the approved mock-ups, for the evidence folder.
const SHOTS = process.env.DESIGN_SHOTS_DIR;
test.describe('screenshots', () => {
  test.skip(!SHOTS, 'set DESIGN_SHOTS_DIR to write the screenshots');
  test.use({ colorScheme: 'light' });

  test('desktop tree, desktop notes with the picker, phone tree', async ({ browser, page }) => {
    const harness = await open(browser, page, { iterations: true, notes: iterationNotes() });
    try {
      await page.setViewportSize({ width: 1280, height: 720 });
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D7"] .tree-node-main').click();
      await expect(page.locator('.design-viewer .cmp-img').first()).toBeVisible();
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${SHOTS}/after-desktop-tree.png` });
      await page.getByRole('button', { name: 'Back to current' }).click();
      await page.getByRole('tab', { name: 'Notes' }).click();
      const chain = page.locator('#chain-D3');
      await chain.getByRole('button', { name: 'Follow up' }).click();
      await chain.getByLabel(/Follow-up on D9/).fill('Slightly darker, like @D');
      await chain.getByRole('listbox', { name: 'Notes matching what you typed' }).getByRole('option', { name: /^D5/ }).click();
      await chain.getByRole('button', { name: '+ Reference a change' }).click();
      await chain.scrollIntoViewIfNeeded();
      await page.waitForTimeout(400);
      await page.screenshot({ path: `${SHOTS}/after-desktop-notes.png` });
      await page.setViewportSize({ width: 390, height: 844 });
      await page.getByRole('navigation', { name: 'Design modes' }).getByRole('button', { name: 'Notes', exact: true }).click();
      await page.getByRole('tab', { name: 'Tree' }).click();
      await page.locator('.tree-node[data-note="D7"] .tree-node-main').click();
      await page.waitForTimeout(700);
      await page.screenshot({ path: `${SHOTS}/after-phone-tree.png` });
    } finally { await harness.stop(); }
  });
});
