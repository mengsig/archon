import { test, expect } from '@playwright/test';
import { startListenerFixture } from './support/listener.js';
import { openApp } from './flows.js';
import { guardConsole } from './support/console-guard.js';
import { textContrastProblems } from './support/contrast.js';

for (const theme of ['light', 'dark']) {
  test(`fresh operator overview loads within two seconds and has readable ${theme} text`, async ({ browser }) => {
    const fixture = await startListenerFixture({ seed: false });
    const context = await browser.newContext({ viewport: { width: 390, height: 844 }, colorScheme: theme, reducedMotion: 'reduce' });
    const page = await context.newPage();
    const problems = guardConsole(page);
    try {
      await openApp(page, fixture, '#/overview');
      const start = performance.now();
      await page.reload();
      await expect(page.locator('.metrics')).toBeVisible({ timeout: 2000 });
      const elapsed = performance.now() - start;
      expect(elapsed).toBeLessThan(2000);
      console.log(`paired overview ${theme}: ${Math.round(elapsed)} ms`);
      expect(await textContrastProblems(page)).toEqual([]);
      expect(await page.evaluate(() => document.getAnimations().length)).toBe(0);
    } finally { await context.close(); await fixture.stop(); }
    expect(problems).toEqual([]);
  });
}

test('a 500-node graph retains readable nodes and pans without rebuilding them', async ({ browser }) => {
  const fixture = await startListenerFixture({ seed: false });
  const context = await browser.newContext({ viewport: { width: 390, height: 844 } });
  const page = await context.newPage();
  const problems = guardConsole(page);
  try {
    await page.routeWebSocket(/\/ws$/, (ws) => {
      const server = ws.connectToServer();
      ws.onMessage((message) => {
        const frame = JSON.parse(message);
        if (frame.cmd !== 'task_graph') return server.send(message);
        const nodes = Array.from({ length: 500 }, (_, i) => ({ id: `qi_perf${i}`, title: `Work item ${i}`, state: 'running', pipeline: 'adaptive', spent_usd: i / 100 }));
        const edges = nodes.slice(1).map((node, i) => ({ from: nodes[Math.floor(i / 3)].id, to: node.id, kind: 'depends_on', satisfied: false }));
        ws.send(JSON.stringify({ frame: 'reply', id: frame.req, result: { Ok: { reply: 'task_graph', data: { nodes, edges, initiatives: [], truncated: false } } } }));
      });
    });
    await openApp(page, fixture, '#/graph');
    await expect(page.locator('.graph-node')).toHaveCount(500);
    await expect(page.getByLabel('Zoom level', { exact: true })).toHaveText('100%');
    const result = await page.locator('.graph-viewport').evaluate(async (svg) => {
      const node = svg.querySelector('.graph-node');
      const world = svg.querySelector('.graph-world');
      const before = world.getAttribute('transform');
      const times = [];
      let last = performance.now();
      for (let i = 0; i < 30; i++) {
        svg.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true }));
        await new Promise(requestAnimationFrame);
        const now = performance.now(); times.push(now - last); last = now;
      }
      return { sameNode: node === svg.querySelector('.graph-node'), moved: world.getAttribute('transform') !== before,
        frames: times.length, medianMs: times.sort((a, b) => a - b)[15] };
    });
    expect(result.sameNode && result.moved).toBe(true);
    expect(result.frames).toBe(30);
    console.log(`500-node pan median frame: ${result.medianMs.toFixed(1)} ms`);
  } finally { await context.close(); await fixture.stop(); }
  expect(problems).toEqual([]);
});
