// A phone paired as an operator does from the app what the operator does
// from the TUI; one paired as an observer is told why it cannot.
import { test as base, expect } from '@playwright/test';
import { startListenerFixture } from './support/listener.js';
import { askOperator } from './support/seed.js';
import { guardConsole } from './support/console-guard.js';

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startListenerFixture({ seed: false });
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

test.describe.configure({ mode: 'serial' });

async function pairAs(page, fixture, role) {
  const args = role === 'observer' ? ['pair', '--observer'] : ['pair'];
  const { code } = await fixture.remote(args);
  await page.goto(`${fixture.baseURL}/pair#code=${code}`);
  await expect(page.getByRole('form', { name: 'Pair this device' })).toBeVisible();
  await page.getByLabel('Device name').fill(`Playwright ${role}`);
  await page.getByRole('button', { name: 'Pair' }).click();
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  await page.goto(`${fixture.baseURL}/#/chat`);
  await expect(page.getByRole('heading', { name: 'Chat', level: 1 })).toBeVisible();
}

async function send(page, line) {
  await page.getByRole('textbox', { name: 'Message' }).fill(line);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  return page.getByRole('log', { name: 'Conversation' }).locator('.from-operator', { hasText: line });
}

async function systemNotes(fixture) {
  const history = await fixture.archond.request({ cmd: 'feed_history', before: null, limit: 100, verbose: false });
  return history.data.filter((item) => item.entry.entry === 'system').map((item) => item.entry.text);
}

async function directives(fixture) {
  const view = await fixture.archond.request({ cmd: 'autonomy', repo: null });
  return view.data.directives.map((directive) => directive.text);
}

test('operator: sets and clears a directive and answers a question from the app', async ({ page, fixture }) => {
  await pairAs(page, fixture, 'operator');
  const set = '/archon apply {"cmd":"set_directive","text":"docs fixes only today","expires_in_hours":null}';
  const setLine = await send(page, set);
  await expect(setLine).toBeVisible();
  await expect(setLine.getByText(/Not sent/)).toHaveCount(0);
  await expect.poll(() => directives(fixture)).toEqual(['docs fixes only today']);

  await send(page, '/archon apply {"cmd":"clear_directive","directive":null}');
  await expect.poll(() => directives(fixture)).toEqual([]);
  await expect.poll(() => systemNotes(fixture)).toContainEqual(expect.stringMatching(/All directives cleared/));

  const question = await askOperator(fixture.archond, 'Ship the docs fix?', ['yes', 'no']);
  await send(page, `/answer ${question.number} yes`);
  await expect.poll(async () => {
    const listed = await fixture.archond.request({ cmd: 'list_questions', open_only: false });
    return listed.data.find((q) => q.id === question.id)?.answer ?? null;
  }).toBe('yes');
  const transcript = page.getByRole('log', { name: 'Conversation' });
  await expect(transcript.getByText(/Not sent/)).toHaveCount(0);
});

test('observer: a change from the app is refused with the reason', async ({ browser, fixture }) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  const problems = guardConsole(page);
  try {
    await pairAs(page, fixture, 'observer');
    const line = await send(page, '/archon apply {"cmd":"set_directive","text":"observer says","expires_in_hours":null}');
    await expect(line).toContainText('Not sent: say changes Archon. This is an observer device: it can only read. Pair it as an operator to act from it.');
    expect(await directives(fixture)).toEqual([]);
  } finally {
    await context.close();
  }
  expect(problems, 'the page logged errors').toEqual([]);
});
