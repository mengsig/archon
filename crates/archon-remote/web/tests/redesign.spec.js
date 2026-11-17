import { test as base, expect } from '@playwright/test';
import { startDevFixture } from './support/fixture.js';
import { openApp, answerQuestion, enqueueViaSheet, globalPauseResume, chat } from './flows.js';
import { enqueue, notify } from './support/seed.js';
import { pipelineFixture, standInGraphs } from './support/graph-fixture.js';
import { textContrastProblems } from './support/contrast.js';
import { guardConsole } from './support/console-guard.js';

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

for (const viewport of [{ width: 390, height: 844 }, { width: 1280, height: 800 }]) {
  for (const theme of ['light', 'dark']) {
    test.describe(`${viewport.width}px ${theme}`, () => {
      test.use({ viewport, colorScheme: theme });
      test('overview, dependency graph and operator actions remain reachable', async ({ page, fixture }) => {
        await openApp(page, fixture, '#/overview');
        await expect(page.locator('.metrics')).toBeVisible();
        await expect(page.getByRole('img', { name: /^Daily spend/ })).toBeVisible();
        await page.getByText('View daily amounts', { exact: true }).click();
        await expect(page.getByRole('table')).toBeVisible();
        const nav = page.getByRole('navigation', { name: 'Sections' });
        for (const name of ['Queue', 'Graph', 'Inbox', 'Chat', 'Overview']) {
          await nav.getByRole('link', { name: new RegExp(`^${name}`) }).click();
          await expect(page.getByRole('heading', { name, exact: true, level: 1 })).toBeVisible();
          expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
          expect(await textContrastProblems(page)).toEqual([]);
        }
        await page.getByRole('link', { name: 'Settings', exact: true }).click();
        await expect(page.getByRole('heading', { name: 'Settings', level: 1 })).toBeVisible();
        await page.goto(`${fixture.baseURL}/#/chat`);
        await page.getByRole('button', { name: 'Commands', exact: true }).click();
        const dialog = page.getByRole('dialog', { name: 'Operator commands' });
        await dialog.getByLabel('Operator command', { exact: true }).fill('/status');
        await dialog.getByRole('button', { name: 'Send command' }).click();
        await expect(dialog.getByRole('status')).toContainText('Command received');
        await dialog.getByRole('button', { name: 'Close', exact: true }).click();
        await answerQuestion(page, fixture);
        await enqueueViaSheet(page, fixture);
        await globalPauseResume(page, fixture);
        await page.goto('about:blank');
        await chat(page, fixture);
      });

      test('dependencies keep typed edges, show states and refresh from feed', async ({ page, fixture }) => {
        const first = await enqueue(fixture.archond, { repo: 'archon', title: 'Foundation', brief: 'Graph fixture' });
        const second = await enqueue(fixture.archond, { repo: 'archon', title: 'Application', brief: 'Graph fixture', dependsOn: first.id });
        const third = await enqueue(fixture.archond, { repo: 'archon', title: 'Release', brief: 'Graph fixture', dependsOn: second.id });
        await openApp(page, fixture, '#/graph');
        await expect(page.locator(`.graph-node[data-node-id="${first.id}"]`)).toHaveAttribute('aria-label', /Foundation/);
        await expect(page.locator(`.graph-edge[data-from="${first.id}"][data-to="${second.id}"]`)).toHaveCount(1);
        await expect(page.locator(`.graph-edge[data-from="${second.id}"][data-to="${third.id}"]`)).toHaveCount(1);
        await fixture.archond.run(['queue', 'cancel', third.id]);
        await expect(page.locator(`.graph-node[data-node-id="${third.id}"]`)).toHaveAttribute('aria-label', /cancelled/);
        const zoom = page.getByLabel('Zoom level', { exact: true });
        await page.getByRole('button', { name: 'Zoom in', exact: true }).click();
        await expect(zoom).toHaveText('120%');
        await page.getByRole('button', { name: 'Fit entire graph', exact: true }).click();
        await page.getByRole('searchbox', { name: 'Search graph' }).fill(first.id);
        await expect(page.locator('.graph-node')).toHaveCount(1);
        const node = page.locator('.graph-node').first();
        await node.focus();
        await page.keyboard.press('Enter');
        await expect(page.getByRole('complementary', { name: 'Selected item' })).toContainText('Foundation');
        await page.getByRole('link', { name: 'Open item', exact: true }).click();
        await expect(page.getByRole('heading', { name: 'Foundation', level: 1 })).toBeVisible();
      });

      test('stack and initiative edges retain their types and satisfaction on updates', async ({ page, fixture }) => {
        const base = (await fixture.archond.request({ cmd: 'get_item', item: fixture.seeded.first.id })).data;
        const data = pipelineFixture(base);
        data.graph = (await fixture.archond.request({ cmd: 'task_graph', filter: { include_terminal_since: '1970-01-01T00:00:00Z' }, limit: 500 })).data;
        const [first, second] = data.graph.nodes;
        data.graph.edges = [{ from: first.id, to: second.id, kind: 'stacked_on', satisfied: false },
          { from: first.id, to: second.id, kind: 'initiative_after', satisfied: false }];
        await standInGraphs(page, data);
        await openApp(page, fixture, '#/graph');
        const edge = page.locator('.graph-edge[data-kind="stacked_on"]');
        await expect(edge).toHaveAttribute('data-satisfied', 'false');
        await expect(edge.locator('.graph-edge-status')).toHaveText('Unsatisfied');
        const initiative = page.locator('.graph-edge[data-kind="initiative_after"]');
        await expect(initiative).toHaveCount(1);
        expect(await edge.locator('path').getAttribute('d')).not.toBe(await initiative.locator('path').getAttribute('d'));
        expect(await edge.locator('path').evaluate((node) => getComputedStyle(node).strokeDasharray)).not.toBe('none');
        data.graph.edges[0].satisfied = true;
        first.state = 'succeeded';
        await notify(fixture.archond, 'progress', 'Prerequisite completed.', first.id);
        await expect(edge).toHaveAttribute('data-satisfied', 'true');
        await expect(edge.locator('.graph-edge-status')).toHaveText('Satisfied');
        await expect(page.locator(`.graph-node[data-node-id="${first.id}"]`)).toHaveAttribute('aria-label', /succeeded/);
      });

      test('live item updates preserve graph focus and an active pan', async ({ page, fixture }) => {
        const base = (await fixture.archond.request({ cmd: 'get_item', item: fixture.seeded.first.id })).data;
        const data = pipelineFixture(base);
        await standInGraphs(page, data);
        await openApp(page, fixture, `#/item/${base.item.id}`);
        await page.getByRole('tab', { name: 'Work', exact: true }).click();
        const node = page.locator('#item-work .graph-node[aria-label^="build · Attempt"]');
        await node.focus();
        data.detail.stages[1].cost_usd = 1.75;
        await notify(fixture.archond, 'progress', 'Stage usage updated.', base.item.id);
        await expect(node.locator('.graph-node-cost')).toHaveText('$1.75');
        await expect(node).toBeFocused();
        const box = await node.boundingBox();
        await page.mouse.move(box.x + 40, box.y + 40);
        await page.mouse.down();
        await page.mouse.move(box.x + 60, box.y + 40);
        expect(await node.evaluate((el) => el.hasPointerCapture(1))).toBe(true);
        data.detail.stages[1].cost_usd = 2;
        await notify(fixture.archond, 'progress', 'Stage usage updated again.', base.item.id);
        await expect(node.locator('.graph-node-cost')).toHaveText('$2.00');
        expect(await node.evaluate((el) => el.hasPointerCapture(1))).toBe(true);
        await page.mouse.up();
      });

      test('the work graph fits every node on its first render', async ({ page, fixture }) => {
        test.skip(viewport.width < 1280, 'a phone keeps the readable scale and pans instead');
        const base = (await fixture.archond.request({ cmd: 'get_item', item: fixture.seeded.first.id })).data;
        await standInGraphs(page, pipelineFixture(base));
        await openApp(page, fixture, `#/item/${base.item.id}`);
        await page.getByRole('tab', { name: 'Work', exact: true }).click();
        await expect(page.locator('#item-work .graph-node')).toHaveCount(10);
        await expect.poll(() => page.locator('#item-work').evaluate((work) => {
          const view = work.querySelector('.graph-viewport').getBoundingClientRect();
          return [...work.querySelectorAll('.graph-node')].filter((node) => {
            const box = node.getBoundingClientRect();
            return box.left < view.left - 1 || box.right > view.right + 1 || box.top < view.top - 1 || box.bottom > view.bottom + 1;
          }).length;
        }), 'nodes outside the viewport').toBe(0);
      });

      test('labels of edges that share endpoints do not overlap', async ({ page, fixture }) => {
        const base = (await fixture.archond.request({ cmd: 'get_item', item: fixture.seeded.first.id })).data;
        const data = pipelineFixture(base);
        data.graph = (await fixture.archond.request({ cmd: 'task_graph', filter: { include_terminal_since: '1970-01-01T00:00:00Z' }, limit: 500 })).data;
        const [first, second] = data.graph.nodes;
        data.graph.edges = [{ from: first.id, to: second.id, kind: 'depends_on', satisfied: false },
          { from: first.id, to: second.id, kind: 'stacked_on', satisfied: false }];
        await standInGraphs(page, data);
        await openApp(page, fixture, '#/graph');
        const edges = page.locator('.graph-edge');
        await expect(edges).toHaveCount(2);
        // Each edge's kind and status rows form one block; the two blocks must not meet.
        const [a, b] = await edges.evaluateAll((groups) => groups.map((group) => {
          const boxes = [...group.querySelectorAll('.graph-edge-label')].map((text) => text.getBoundingClientRect());
          return { left: Math.min(...boxes.map((box) => box.left)), right: Math.max(...boxes.map((box) => box.right)),
            top: Math.min(...boxes.map((box) => box.top)), bottom: Math.max(...boxes.map((box) => box.bottom)) };
        }));
        const overlap = a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
        expect(overlap, `labels at ${JSON.stringify([a, b])}`).toBe(false);
      });

      test('work and agent graphs show attempts, slots, gates, model, cost and live descendants', async ({ page, fixture }) => {
        const base = (await fixture.archond.request({ cmd: 'get_item', item: fixture.seeded.first.id })).data;
        const data = pipelineFixture(base);
        await standInGraphs(page, data);
        await openApp(page, fixture, `#/item/${base.item.id}`);
        await page.getByRole('tab', { name: 'Work', exact: true }).click();
        await expect(page.locator('#item-work .graph-node')).toHaveCount(10);
        await expect(page.locator('#item-work .graph-node[aria-label*="Attempt 2 · Slot 2"]')).toHaveCount(1);
        await expect(page.locator('#item-work .graph-node[aria-label*="proposal gate"]')).toHaveCount(1);
        const stage = page.locator('#item-work .graph-node[aria-label^="build · Attempt"]');
        await stage.focus();
        await page.keyboard.press('Enter');
        await expect(page.getByRole('dialog', { name: 'build', exact: true })).toContainText('Verified stage output');
        expect(data.requests.find((request) => request.cmd === 'get_stage').run).toBe(data.plan.run);
        await page.getByRole('button', { name: 'Close', exact: true }).click();
        await expect(stage).toBeFocused();
        await page.getByRole('tab', { name: 'Agents', exact: true }).click();
        const child = page.locator('#item-agents .graph-node[aria-label^="agt_reviewer ·"]');
        await expect(child).toHaveAttribute('aria-label', /fable-5.2 · high/);
        await expect(child).toHaveAttribute('aria-label', /live/);
        await expect(child).toHaveAttribute('aria-label', /\$0.30/);
        await expect(page.locator('#item-agents .graph-edge[aria-label^="agt_reviewer → agt_check"]')).toHaveCount(1);
        data.agentStates.agents[0].state = 'completed';
        await notify(fixture.archond, 'progress', 'The delegated reviewer finished.', base.item.id);
        await expect(child).toHaveAttribute('aria-label', /completed/);
        expect(await textContrastProblems(page)).toEqual([]);
        data.stateError = 'Agent backend is unavailable';
        await notify(fixture.archond, 'progress', 'Agent backend disconnected.', base.item.id);
        await expect(page.locator('#item-agents')).toContainText(data.stateError);
        await expect(child).toHaveAttribute('aria-label', /State unavailable/);
        delete data.stateError;
        await page.locator('#item-agents').getByRole('button', { name: 'Try again', exact: true }).click();
        await expect(child).toHaveAttribute('aria-label', /completed/);
        await page.emulateMedia({ reducedMotion: 'reduce' });
        await page.getByRole('tab', { name: 'Work', exact: true }).click();
        expect(await page.evaluate(() => document.getAnimations().filter((animation) => animation.playState === 'running').length)).toBe(0);
        expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
      });
    });
  }
}
