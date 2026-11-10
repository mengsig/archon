// Every flow against a real archond through the dev bridge.
import { test as base, expect } from '@playwright/test';
import { FLOWS, openApp, ensureRepo } from './flows.js';
import { enqueue, notify, askOperator } from './support/seed.js';
import { startDevFixture } from './support/fixture.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startDevFixture();
    try {
      await use(fixture);
    } finally {
      await fixture.stop();
    }
  }, { scope: 'worker', timeout: 120_000 }],
  page: async ({ page }, use) => {
    const problems = guardConsole(page);
    await use(page);
    expect(problems, 'the page logged errors').toEqual([]);
  },
});

for (const [name, flow] of Object.entries(FLOWS)) {
  test(name, async ({ page, fixture }) => {
    await flow(page, fixture);
  });
}

// Behaviour the bridge can act out but a real listener cannot be asked to.
test('reconnects and resubscribes after the socket drops', async ({ page, fixture }) => {
  await openApp(page, fixture);
  await expect(page.locator('.item-row').first()).toBeVisible();
  fixture.dev.closeClients(1012, 'restarting');
  const title = `Arrived while reconnecting ${Date.now().toString(36)}`;
  await enqueue(fixture.archond, { repo: 'archon', title, brief: 'Shows up once the app is back.' });
  await expect(page.getByRole('link', { name: new RegExp(`^${title}`) })).toBeVisible({ timeout: 20_000 });
  await expect(page.locator('.conn-banner')).not.toHaveClass(/is-shown/);
});

// Refuses the commands `refuse` names while it returns true for them.
async function refuseSome(page, refuse) {
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    ws.onMessage((message) => {
      const frame = JSON.parse(message);
      if (!refuse(frame)) return server.send(message);
      ws.send(JSON.stringify({ frame: 'reply', id: frame.req, result: { Err: { code: 'internal', message: `${frame.cmd} refused by the test` } } }));
    });
  });
}

test('load failures stay in view over what was loaded before', async ({ page, fixture }) => {
  let statusLoads = 0;
  await refuseSome(page, (frame) => ['feed_history', 'repo_list'].includes(frame.cmd) || (frame.cmd === 'status' && ++statusLoads > 1));
  const alert = (text) => page.getByRole('alert').filter({ hasText: text });
  const go = (name) => page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name }).click();
  await openApp(page, fixture, '#/inbox');
  const notifications = page.getByRole('region', { name: 'Notifications' });
  await expect(notifications.getByRole('alert')).toContainText('feed_history refused by the test');
  await expect(notifications.getByText('No notifications yet.')).toHaveCount(0);
  await go('Overview');
  await expect(alert('status refused by the test')).toBeVisible();
  await expect(page.locator('.hero-card')).toBeVisible();
  await go('Queue');
  await expect(alert('Could not load repositories: repo_list refused by the test')).toBeVisible();
  await page.locator('.item-row').first().click();
  await expect(page.locator('.item-hero')).toBeVisible();
  await expect(alert('Could not load the status: status refused by the test')).toBeVisible();
  await expect(alert('Could not load repositories')).toBeVisible();
  await go('Chat');
  await expect(alert('Could not load the status')).toBeVisible();
});

test('the new-item sheet shows a failed repository list and fills in once it loads', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  let refuseRepos = true;
  await refuseSome(page, (frame) => refuseRepos && frame.cmd === 'repo_list');
  await openApp(page, fixture);
  await page.getByRole('button', { name: 'New item' }).click();
  const sheet = page.getByRole('dialog', { name: 'New item' });
  const failed = sheet.getByRole('alert').filter({ hasText: 'Could not load repositories: repo_list refused by the test' });
  await expect(failed).toBeVisible();
  await sheet.getByLabel('Title').fill('Kept across the retry');
  refuseRepos = false;
  await failed.getByRole('button', { name: 'Try again' }).click();
  await expect(failed).toHaveCount(0);
  await expect(sheet.getByLabel('Repository')).toHaveValue('archon');
  await expect(sheet.getByLabel('Title')).toHaveValue('Kept across the retry');
});

test('the new-item sheet retries a failed pipeline listing in place', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  let refusePipelines = true;
  await refuseSome(page, (frame) => refusePipelines && frame.cmd === 'list_pipelines');
  await openApp(page, fixture);
  await page.getByRole('button', { name: 'New item' }).click();
  const sheet = page.getByRole('dialog', { name: 'New item' });
  const failed = sheet.getByRole('alert').filter({ hasText: 'Could not list pipelines: list_pipelines refused by the test' });
  await expect(failed).toBeVisible();
  refusePipelines = false;
  await failed.getByRole('button', { name: 'Try again' }).click();
  await expect(failed).toHaveCount(0);
  await expect(sheet.getByRole('combobox', { name: 'Pipeline' }).locator('option[value="direct"]')).toHaveCount(1);
});

test('a failed artifact page read is retried inside its sheet', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: `Artifacts ${Date.now().toString(36)}`, brief: 'Has one artifact.' });
  let reads = 0;
  // The listing and the page are acted out: publishing needs a running stage.
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    const answer = (frame, result) => ws.send(JSON.stringify({ frame: 'reply', id: frame.req, result }));
    ws.onMessage((message) => {
      const frame = JSON.parse(message);
      if (frame.cmd === 'list_artifacts') {
        return answer(frame, { Ok: { reply: 'artifacts', data: { item: item.id, files: [{ name: 'notes.txt', bytes: 5 }], omitted: 0 } } });
      }
      if (frame.cmd !== 'read_artifact') return server.send(message);
      reads += 1;
      if (reads === 1) return answer(frame, { Err: { code: 'internal', message: 'read_artifact refused by the test' } });
      return answer(frame, { Ok: { reply: 'artifact_page', data: { text: 'hello', next_offset: null, total_bytes: 5 } } });
    });
  });
  await openApp(page, fixture, `#/item/${item.id}`);
  await page.getByRole('button', { name: /notes\.txt/ }).click();
  const sheet = page.getByRole('dialog', { name: 'notes.txt' });
  const failed = sheet.getByRole('alert').filter({ hasText: 'read_artifact refused by the test' });
  await expect(failed).toBeVisible();
  await failed.getByRole('button', { name: 'Try again' }).click();
  await expect(failed).toHaveCount(0);
  await expect(sheet.getByLabel('Contents of notes.txt')).toHaveText('hello');
});

test('a failed feed setup is retried until the app is live again', async ({ page, fixture }) => {
  let refuseFeed = true;
  await refuseSome(page, (frame) => refuseFeed && frame.cmd === 'feed_history');
  await openApp(page, fixture, '#/inbox');
  const notifications = page.getByRole('region', { name: 'Notifications' });
  await expect(notifications.getByRole('alert')).toContainText('feed_history refused by the test');
  refuseFeed = false;
  await expect(notifications.getByRole('alert')).toHaveCount(0, { timeout: 10_000 });
  const text = `Live again ${Date.now().toString(36)}`;
  await notify(fixture.archond, 'outcome', text);
  await expect(notifications.getByText(text)).toBeVisible();
});

test('a failed item reload stays in view when other data changes', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: `Reload me ${Date.now().toString(36)}`, brief: 'Its reload fails.' });
  const refuse = { item: false, status: false };
  let statusRefused = 0;
  await refuseSome(page, (frame) => {
    if (frame.cmd === 'status' && refuse.status) statusRefused += 1;
    return (frame.cmd === 'get_item' && refuse.item) || (frame.cmd === 'status' && refuse.status);
  });
  await openApp(page, fixture, `#/item/${item.id}`);
  await expect(page.getByRole('heading', { name: item.title, level: 1 })).toBeVisible();
  refuse.item = true;
  await notify(fixture.archond, 'progress', `Stage settled for ${item.id}.`, item.id);
  const failed = page.getByRole('alert').filter({ hasText: 'get_item refused by the test' });
  await expect(failed).toBeVisible();
  refuse.status = true;
  await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
  await expect.poll(() => statusRefused).toBeGreaterThan(0);
  await page.waitForTimeout(500);
  await expect(failed).toBeVisible();
  await expect(page.locator('.item-hero')).toHaveCount(0);
});

test('a revoked device goes back to pairing', async ({ page, fixture }) => {
  await openApp(page, fixture);
  fixture.dev.closeClients(4401, 'revoked');
  await expect(page.getByRole('form', { name: 'Pair this device' })).toBeVisible();
});

test('the theme override sticks and reduced motion stills the page', async ({ page, fixture }) => {
  await page.emulateMedia({ colorScheme: 'light', reducedMotion: 'reduce' });
  await openApp(page, fixture, '#/settings');
  await page.getByRole('radio', { name: 'Dark' }).click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.reload();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
  await page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name: 'Queue' }).click();
  const duration = await page.locator('.view').first().evaluate((el) => getComputedStyle(el).animationDuration);
  expect(Number.parseFloat(duration)).toBeLessThan(0.01);
});

// Markdown the daemon would carry, laid into real replies: the orchestrator
// and stage summaries cannot be written by a test while Aether stays down.
const ORCHESTRATOR_MD = [
  '## Plan', '', 'This is **ABCD** with `inline code` and a [link](https://example.com/docs).', '',
  '- first', '- second', '  - nested', '', '| Name | Cost |', '|:--|--:|', '| render | $1 |', '',
  '```js', 'const **raw** = 1;', '```', '', '<script>window.pwned = 1</script>', '', '[evil](javascript:window.pwned=2)',
].join('\n');
const SUMMARY_MD = [
  '# Result', '', 'Everything is **done**.', '', '1. one', '2. two', '', '> a quoted `note`', '', '| a | b |', '|---|---|', '| 1 | 2 |', '',
  `A long tail paragraph. ${'More words follow here. '.repeat(30)}`, '', 'Final block with _emphasis_.',
].join('\n');

async function laceReplies(page, edit) {
  await page.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    ws.onMessage((message) => server.send(message));
    server.onMessage((message) => {
      const frame = JSON.parse(message);
      if (frame.frame === 'reply' && frame.result?.Ok) edit(frame.result.Ok);
      ws.send(JSON.stringify(frame));
    });
  });
}

test('Markdown renders as elements in chat and stage summaries, never as markup', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: `Markdown ${Date.now().toString(36)}`, brief: '# Brief\n\nSome **bold** brief text.' });
  await laceReplies(page, (reply) => {
    if (reply.reply === 'feed') reply.data.push({ seq: 1_000_000, at: new Date().toISOString(), entry: { entry: 'orchestrator', text: ORCHESTRATOR_MD } });
    if (reply.reply === 'item_detail' && reply.data.item.id === item.id) {
      reply.data.stages = [{
        id: 'stage_md', run: reply.data.runs?.[0]?.id ?? 'run_md', stage_id: 'implement', attempt: 1, fan_index: 0, state: { state: 'succeeded' },
        started_at: new Date(Date.now() - 60_000).toISOString(), finished_at: new Date().toISOString(), cost_usd: 0, model_segments: [], summary: SUMMARY_MD,
      }];
    }
  });

  await openApp(page, fixture, '#/chat');
  const bubble = page.locator('.from-orchestrator .bubble').last();
  await expect(bubble.locator('strong')).toHaveText('ABCD');
  await expect(bubble.getByRole('heading', { name: 'Plan', level: 2 })).toBeVisible();
  await expect(bubble.locator('li')).toHaveCount(3);
  await expect(bubble.locator('li ul li')).toHaveText('nested');
  await expect(bubble.locator('code', { hasText: 'inline code' })).toBeVisible();
  // Code spans keep their spaces; the prose around them collapses.
  expect(await bubble.locator('code', { hasText: 'inline code' }).evaluate((el) => getComputedStyle(el).whiteSpace)).toBe('pre-wrap');
  await expect(bubble.locator('pre code')).toHaveText('const **raw** = 1;');
  await expect(bubble.locator('table th')).toHaveText(['Name', 'Cost']);
  await expect(bubble.locator('table td').last()).toHaveText('$1');
  await expect(bubble.getByRole('link', { name: 'link' })).toHaveAttribute('href', 'https://example.com/docs');
  await expect(bubble.getByRole('link', { name: 'link' })).toHaveAttribute('rel', 'noopener noreferrer');
  await expect(bubble).toContainText('<script>window.pwned = 1</script>');
  await expect(bubble).not.toContainText('**ABCD**');
  await expect(bubble).not.toContainText('## Plan');
  await expect(page.locator('script:not([src])')).toHaveCount(0);
  await expect(page.locator('a[href^="javascript:" i]')).toHaveCount(0);
  expect(await page.evaluate(() => window.pwned)).toBeUndefined();
  // Wide tables and code scroll inside their block instead of widening the page.
  expect(await bubble.locator('.md-scroll').evaluate((el) => getComputedStyle(el).overflowX)).toBe('auto');
  expect(await bubble.locator('pre').evaluate((el) => getComputedStyle(el).overflowX)).toBe('auto');

  await page.goto(`${fixture.baseURL}/#/item/${item.id}`);
  await expect(page.getByRole('heading', { name: 'Brief', level: 1 })).toBeVisible();
  const summary = page.locator('.timeline-summary');
  await expect(summary.getByRole('heading', { name: 'Result', level: 1 })).toBeVisible();
  await expect(summary.locator('strong')).toHaveText('done');
  await expect(summary.locator('li')).toHaveCount(2);
  await expect(summary.locator('blockquote code')).toHaveText('note');
  await expect(summary.locator('table')).toBeVisible();
  await expect(summary).not.toContainText('Final block');
  await page.getByRole('button', { name: 'Show all' }).click();
  await expect(summary).toContainText('Final block with');
  await expect(summary.locator('em')).toHaveText('emphasis');
  await expect(page.getByRole('button', { name: 'Show less' })).toBeVisible();
  await page.getByRole('button', { name: 'Show less' }).click();
  await expect(summary).not.toContainText('Final block');
  await expect(page.locator('script:not([src])')).toHaveCount(0);
});

const WIDE = 'x'.repeat(160);
const WIDE_MD = ['Wide content:', '', '```', WIDE, '```', '', '| a | b |', '|--|--|', `| ${WIDE} | cell |`].join('\n');
// Wide code at the deepest nesting the parser allows, in lists and in quotes.
const DEEP_MD = [
  ...Array.from({ length: 16 }, (_, i) => `${'  '.repeat(i)}- level ${i + 1}`), ...['```', WIDE, '```'].map((l) => '  '.repeat(16) + l), '',
  ...['Deep quote', '```', WIDE, '```'].map((l) => '> '.repeat(15) + l),
].join('\n');

// The page never scrolls sideways, and each wide box scrolls inside the viewport.
async function expectContained(page, scope) {
  const page_ = await page.evaluate(() => ({ scroll: document.documentElement.scrollWidth, client: document.documentElement.clientWidth }));
  expect(page_.scroll, 'the page scrolls sideways').toBeLessThanOrEqual(page_.client);
  const boxes = scope.locator('.md-pre, .md-scroll');
  expect(await boxes.count()).toBeGreaterThan(0);
  for (const box of await boxes.all()) {
    const size = await box.evaluate((el) => ({ right: el.getBoundingClientRect().right, scroll: el.scrollWidth, client: el.clientWidth, width: innerWidth }));
    expect(size.right).toBeLessThanOrEqual(size.width);
    expect(size.scroll).toBeGreaterThan(size.client);
  }
}

test('wide Markdown code and tables scroll inside their block, never the page', async ({ page, fixture }) => {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: `Wide ${Date.now().toString(36)}`, brief: WIDE_MD });
  await notify(fixture.archond, 'outcome', WIDE_MD);
  await askOperator(fixture.archond, WIDE_MD);
  await laceReplies(page, (reply) => {
    if (reply.reply === 'feed') {
      reply.data.push({ seq: 1_000_001, at: new Date().toISOString(), entry: { entry: 'orchestrator', text: WIDE_MD } });
      reply.data.push({ seq: 1_000_002, at: new Date().toISOString(), entry: { entry: 'orchestrator', text: DEEP_MD } });
    }
    if (reply.reply === 'item_detail' && reply.data.item.id === item.id) {
      reply.data.stages = [{
        id: 'stage_wide', run: reply.data.runs?.[0]?.id ?? 'run_wide', stage_id: 'implement', attempt: 1, fan_index: 0, state: { state: 'failed', error: WIDE_MD },
        started_at: new Date(Date.now() - 60_000).toISOString(), finished_at: new Date().toISOString(), cost_usd: 0, model_segments: [], summary: WIDE_MD,
      }];
    }
  });

  await openApp(page, fixture, '#/chat');
  const wide = page.locator('.from-orchestrator .bubble').filter({ hasText: 'Wide content' }).last();
  await expect(wide.locator('table')).toBeVisible();
  await expectContained(page, wide);
  const deep = page.locator('.from-orchestrator .bubble').filter({ hasText: 'level 16' }).last();
  await expect(deep.getByText('Deep quote')).toBeVisible();
  await expectContained(page, deep);
  for (const box of await deep.locator('.md-pre').all()) {
    const inside = await box.evaluate((el) => el.getBoundingClientRect().right <= el.closest('.bubble').getBoundingClientRect().right);
    expect(inside, 'deep code stays inside its bubble').toBe(true);
  }
  await expectContained(page, page.locator('.feed-note-outcome').filter({ hasText: 'Wide content' }).last());

  await page.goto(`${fixture.baseURL}/#/inbox`);
  await expect(page.locator('.question-text table').first()).toBeVisible();
  await expectContained(page, page.locator('.question-card').filter({ hasText: 'Wide content' }).first());

  await page.goto(`${fixture.baseURL}/#/item/${item.id}`);
  await page.getByRole('button', { name: 'Show full brief' }).click();
  await page.getByRole('button', { name: 'Show all' }).click();
  await expect(page.locator('.timeline-summary table')).toBeVisible();
  await expectContained(page, page.locator('.item-detail'));
});
