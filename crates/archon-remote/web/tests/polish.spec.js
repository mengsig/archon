// Layout and legibility regressions found in the 1.6.2 validation pass.
import { test as base, expect } from '@playwright/test';
import { startDevFixture } from './support/fixture.js';
import { openApp } from './flows.js';
import { standInConversation } from './support/conversation.js';
import { pipelineFixture, standInGraphs } from './support/graph-fixture.js';
import { guardConsole } from './support/console-guard.js';

// The 9–10 px labels were raised to 12 px; 11 px stays the floor for secondary text.
const RAISED_LABELS = '.screen-eyebrow, .tab, .chip, .metric-label, .metric-note, .chart-axis, .nav-eyebrow, .nav-version, .graph-edge-label, .feed-line-label';
const MIN_TEXT_PX = 11;
const MIN_LABEL_PX = 12;

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startDevFixture();
    try { await use(fixture); } finally { await fixture.stop(); }
  }, { scope: 'worker', timeout: 120000 }],
  page: async ({ page }, use) => {
    const problems = guardConsole(page);
    await use(page);
    expect(problems).toEqual([]);
  },
});

// Icon-only controls: the glyph against the button, and a filled button against what is behind it.
function iconButtonContrast(page) {
  return page.evaluate(() => {
    const rgba = (value) => {
      const parts = value.match(/[\d.]+/g)?.map(Number);
      return parts?.length >= 3 ? [parts[0], parts[1], parts[2], parts[3] ?? 1] : [0, 0, 0, 0];
    };
    const blend = (front, back) => front.slice(0, 3).map((c, i) => c * front[3] + back[i] * (1 - front[3]));
    const luminance = (color) => color.map((c) => c / 255)
      .map((c) => (c <= .04045 ? c / 12.92 : ((c + .055) / 1.055) ** 2.4))
      .reduce((sum, c, i) => sum + c * [.2126, .7152, .0722][i], 0);
    const ratio = (a, b) => (Math.max(luminance(a), luminance(b)) + .05) / (Math.min(luminance(a), luminance(b)) + .05);
    const backdrop = (element) => {
      const layers = [];
      for (let node = element; node; node = node.parentElement) layers.push(rgba(getComputedStyle(node).backgroundColor));
      return layers.reverse().reduce((back, front) => blend(front, back), [255, 255, 255]);
    };
    const controls = [...document.querySelectorAll('button[aria-label], a[aria-label]')]
      .filter((el) => el.checkVisibility() && el.textContent.trim() === '' && el.querySelector('svg'));
    return controls.map((el) => {
      const fill = backdrop(el);
      const behind = backdrop(el.parentElement);
      return { label: el.getAttribute('aria-label'), glyph: +ratio(blend(rgba(getComputedStyle(el).color), fill), fill).toFixed(2),
        filled: el.classList.contains('icon-button-filled'), edge: +ratio(fill, behind).toFixed(2) };
    });
  });
}

// Visible text under `minimum` px among `selector`'s matches, ignoring screen-reader-only text.
function smallText(page, selector, minimum) {
  return page.evaluate(([sel, min]) => [...document.querySelectorAll(sel)]
    .filter((el) => el.checkVisibility() && !el.closest('.sr-only')
      && [...el.childNodes].some((n) => n.nodeType === Node.TEXT_NODE && n.textContent.trim()))
    .filter((el) => parseFloat(getComputedStyle(el).fontSize) < min)
    .map((el) => `${el.className?.baseVal ?? el.className}: ${el.textContent.trim().slice(0, 30)} (${getComputedStyle(el).fontSize})`), [selector, minimum]);
}

for (const viewport of [{ width: 390, height: 844 }, { width: 1280, height: 800 }]) {
  for (const theme of ['light', 'dark']) {
    test.describe(`${viewport.width}px ${theme}`, () => {
      test.use({ viewport, colorScheme: theme });

      test('icon-only buttons, the send answer button first, meet AA contrast', async ({ page, fixture }) => {
        const audit = async (where) => {
          const controls = await iconButtonContrast(page);
          for (const control of controls) {
            expect(control.glyph, `${where}: ${control.label} glyph`).toBeGreaterThanOrEqual(4.5);
            if (control.filled) expect(control.edge, `${where}: ${control.label} against its card`).toBeGreaterThanOrEqual(3);
          }
          return controls.map((control) => control.label);
        };
        await openApp(page, fixture, '#/inbox');
        await expect(page.getByRole('button', { name: 'Send answer' }).first()).toBeVisible();
        expect(await audit('inbox')).toContain('Send answer');
        await openApp(page, fixture, '#/graph');
        await expect(page.locator('.graph-node').first()).toBeVisible();
        await audit('graph');
        await openApp(page, fixture, '#/chat');
        await expect(page.locator('main h1').first()).toBeVisible();
        await audit('chat');
        await openApp(page, fixture, '#/queue');
        await page.getByRole('button', { name: 'New item' }).click();
        await expect(page.getByRole('dialog', { name: 'New item' })).toBeVisible();
        expect(await audit('new item sheet')).toContain('Close');
      });

      test('raised labels stay at least 12 px and no text drops under 11 px', async ({ page, fixture }) => {
        for (const hash of ['#/overview', '#/queue', '#/inbox', '#/graph', `#/item/${fixture.seeded.first.id}`]) {
          await openApp(page, fixture, hash);
          await expect(page.locator('main h1').first()).toBeVisible();
          await expect(page.locator('.skeleton')).toHaveCount(0);
          expect(await smallText(page, RAISED_LABELS, MIN_LABEL_PX), hash).toEqual([]);
          expect(await smallText(page, 'body *', MIN_TEXT_PX), hash).toEqual([]);
        }
      });

      test('queue chips wrap or truncate inside the screen', async ({ page, fixture }) => {
        await openApp(page, fixture, '#/queue');
        await expect(page.locator('.item-row').first()).toBeVisible();
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
        const strip = page.getByRole('group', { name: 'Filter by state' });
        expect(await strip.evaluate((el) => el.scrollWidth <= el.clientWidth), 'the filter strip scrolls sideways').toBe(true);
        const outside = await page.locator('.chip, .filter-chip').evaluateAll((chips) => chips
          .filter((chip) => chip.checkVisibility())
          .filter((chip) => { const box = chip.getBoundingClientRect(); return box.left < 0 || box.right > innerWidth; }).length);
        expect(outside).toBe(0);
      });

      test('metric values share a baseline and action labels keep to one line', async ({ page, fixture }) => {
        await openApp(page, fixture, '#/overview');
        const rows = await page.locator('.metric').evaluateAll((tiles) => {
          const byRow = new Map();
          for (const tile of tiles) {
            const top = Math.round(tile.getBoundingClientRect().top);
            const value = tile.querySelector('.metric-value').getBoundingClientRect().bottom;
            byRow.set(top, [...(byRow.get(top) ?? []), value]);
          }
          return [...byRow.values()];
        });
        expect(rows.flat().length).toBeGreaterThan(1);
        for (const bottoms of rows) expect(Math.max(...bottoms) - Math.min(...bottoms)).toBeLessThanOrEqual(1);
        await openApp(page, fixture, `#/item/${fixture.seeded.first.id}`);
        const labels = page.locator('.action-label');
        await expect(labels.filter({ hasText: 'Pause pipeline' })).toBeVisible();
        const lines = await labels.evaluateAll((spans) => spans.map((span) => {
          const range = document.createRange();
          range.selectNodeContents(span);
          return { text: span.textContent, lines: new Set([...range.getClientRects()].map((r) => Math.round(r.top))).size };
        }));
        for (const label of lines) expect(label.lines, label.text).toBe(1);
      });

      test('chat posts sit at the left edge of the transcript', async ({ page, fixture }) => {
        await standInConversation(page, [{ kind: 'operator', text: 'Status?' },
          { kind: 'post', text: 'The listener design is **approved**.' }]);
        await openApp(page, fixture, '#/chat');
        const post = page.locator('.feed-line.feed-post');
        await expect(post).toBeVisible();
        const [line, list] = await Promise.all([post.boundingBox(), post.locator('..').boundingBox()]);
        expect(line.x - list.x).toBeLessThanOrEqual(1);
        expect(await post.evaluate((el) => getComputedStyle(el).textAlign)).toMatch(/^(left|start)$/);
      });
    });
  }
}

test.describe('desktop sheets and graph detail', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('the new item submit stays in view without scrolling the sheet', async ({ page, fixture }) => {
    await openApp(page, fixture, '#/queue');
    await page.getByRole('button', { name: 'New item' }).click();
    const sheet = page.getByRole('dialog', { name: 'New item' });
    await expect(sheet).toBeVisible();
    const submit = sheet.getByRole('button', { name: 'Queue item' });
    await expect(submit).toBeInViewport({ ratio: 1 });
    await sheet.getByLabel('Brief').fill('A longer brief.\n'.repeat(30));
    await expect(submit).toBeInViewport({ ratio: 1 });
  });

  test('the selected graph item shows its id, pipeline, state and dependencies', async ({ page, fixture }) => {
    const { first } = fixture.seeded;
    const base = (await fixture.archond.request({ cmd: 'get_item', item: first.id })).data;
    const data = pipelineFixture(base);
    data.graph = (await fixture.archond.request({ cmd: 'task_graph', filter: {}, limit: 500 })).data;
    data.graph.nodes.find((node) => node.id === first.id).pr = { host: 'github.com', repo: 'o/r', number: 9, kind: 'pull_request' };
    await standInGraphs(page, data);
    await openApp(page, fixture, '#/graph');
    await page.locator(`.graph-node[data-node-id="${first.id}"]`).click();
    const panel = page.getByRole('complementary', { name: 'Selected item' });
    await expect(panel).toBeVisible();
    await expect(panel.locator('.graph-detail-facts')).toContainText(first.id);
    for (const fact of ['Item', 'Pipeline', 'State']) await expect(panel.getByText(fact, { exact: true })).toBeVisible();
    await expect(panel.getByRole('heading', { name: 'Dependents' })).toBeVisible();
    await expect(panel.getByRole('link', { name: /Pair devices with a one-time code/ })).toBeVisible();
    await expect(panel.getByRole('link', { name: 'o/r#9', exact: true })).toHaveAttribute('href', 'https://github.com/o/r/pull/9');
  });
});
