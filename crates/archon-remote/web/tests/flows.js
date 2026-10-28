// The app's user flows, each `(page, fixture)` with
// fixture = { baseURL, archond, pairCode(): Promise<string> }, so the same
// flows run against the dev bridge and against the real remote listener.
import { expect } from '@playwright/test';
import { formatPairingCode } from '../src/core/pairing-code.js';
import { addRepo, askOperator, enqueue, notify } from './support/seed.js';

function unique(label) {
  return `${label} ${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
}

async function pairWithForm(page, fixture, deviceName) {
  const code = page.getByLabel('Pairing code');
  if ((await code.inputValue()) === '') await code.fill(await fixture.pairCode());
  const name = page.getByLabel('Device name');
  await expect(name).not.toHaveValue('');
  if (deviceName) await name.fill(deviceName);
  await page.getByRole('button', { name: 'Pair' }).click();
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
}

// Opens the app paired: claims the code if this browser is not paired yet.
export async function openApp(page, fixture, hash = '#/queue') {
  await page.goto(`${fixture.baseURL}/${hash}`);
  const tabs = page.getByRole('navigation', { name: 'Sections' });
  const pairing = page.getByRole('form', { name: 'Pair this device' });
  await expect(tabs.or(pairing)).toBeVisible();
  if (await pairing.isVisible()) {
    await pairWithForm(page, fixture, 'Playwright phone');
    if (hash !== '#/queue') await page.goto(`${fixture.baseURL}/${hash}`);
  }
  await expect(tabs).toBeVisible();
}

export async function ensureRepo(fixture, name) {
  const repos = await fixture.archond.request({ cmd: 'repo_list' });
  if (!repos.data.some((repo) => repo.name === name)) await addRepo(fixture.archond, name);
}

export async function pair(page, fixture) {
  const code = await fixture.pairCode();
  await page.goto(`${fixture.baseURL}/pair#code=${code}`);
  await expect(page.getByRole('form', { name: 'Pair this device' })).toBeVisible();
  // The code leaves the address bar before anything else happens.
  expect(new URL(page.url()).hash).toBe('');
  await expect(page.getByLabel('Pairing code')).toHaveValue(formatPairingCode(code));
  await pairWithForm(page, fixture, 'Playwright phone');
  await expect(page.getByRole('heading', { name: 'Queue', level: 1 })).toBeVisible();
  await page.getByRole('navigation', { name: 'Sections' }).getByRole('link', { name: 'Overview' }).click();
  await page.getByRole('link', { name: 'Settings' }).click();
  await expect(page.getByText('Playwright phone')).toBeVisible();
}

export async function viewQueueAndItem(page, fixture) {
  await ensureRepo(fixture, 'archon');
  const title = unique('Look at the queue');
  await enqueue(fixture.archond, { repo: 'archon', title, brief: 'A row to open from the queue.', priority: 'high' });
  await openApp(page, fixture);
  const row = page.getByRole('link', { name: new RegExp(`^${title}`) });
  await expect(row).toBeVisible();
  await expect(row).toContainText('archon');
  await expect(row).toContainText('High');
  const priority = page.getByRole('combobox', { name: 'Priority' });
  await priority.selectOption('low');
  await expect(row).toBeHidden();
  await priority.selectOption('high');
  await expect(row).toBeVisible();
  await page.getByRole('combobox', { name: 'Pipeline' }).selectOption('direct');
  await expect(row).toBeVisible();
  await page.getByRole('button', { name: /^Held up/ }).click();
  await expect(row).toBeVisible();
  await page.getByRole('button', { name: /^Active/ }).click();

  await page.getByRole('searchbox', { name: 'Search items' }).fill(title.split(' ').at(-1));
  await expect(page.locator('.item-row')).toHaveCount(1);
  await row.click();
  await expect(page.getByRole('heading', { name: title, level: 1 })).toBeVisible();
  await expect(page.getByText('A row to open from the queue.')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Stages' })).toBeVisible();
  await expect(page.getByRole('group', { name: 'Item actions' }).getByRole('button', { name: 'Cancel' })).toBeVisible();
  await page.getByRole('button', { name: 'Back to queue' }).click();
  await expect(page.getByRole('heading', { name: 'Queue', level: 1 })).toBeVisible();
}

export async function chat(page, fixture) {
  const line = unique('Hello from the phone');
  await openApp(page, fixture, '#/chat');
  await expect(page.getByRole('heading', { name: 'Chat', level: 1 })).toBeVisible();
  await page.getByRole('textbox', { name: 'Message' }).fill(line);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  const transcript = page.getByRole('log', { name: 'Conversation' });
  await expect(transcript.locator('.from-operator', { hasText: line })).toBeVisible();
  await expect(transcript.getByText(/Not sent/)).toHaveCount(0);
  await expect(transcript.locator('.from-operator.is-pending', { hasText: line })).toHaveCount(0);

  const history = await fixture.archond.request({ cmd: 'feed_history', before: null, limit: 50, verbose: false });
  expect(history.data.some((item) => item.entry.entry === 'operator' && item.entry.text === line)).toBe(true);
}

export async function answerQuestion(page, fixture) {
  const text = unique('Merge the docs change today?');
  const question = await askOperator(fixture.archond, text, ['approve', 'reject']);
  await openApp(page, fixture, '#/inbox');
  const card = page.locator('.question-card', { hasText: text });
  await expect(card).toBeVisible();
  await card.getByRole('button', { name: 'approve' }).click();
  await expect(card).toHaveCount(0);
  await expect(page.getByRole('list', { name: 'Answered questions' })).toContainText(text);

  const listed = await fixture.archond.request({ cmd: 'list_questions', open_only: false });
  const answered = listed.data.find((q) => q.id === question.id);
  expect(answered?.answer).toBe('approve');
}

export async function enqueueViaSheet(page, fixture) {
  await ensureRepo(fixture, 'archon');
  const title = unique('Queued from the sheet');
  await openApp(page, fixture);
  await page.getByRole('button', { name: 'New item' }).click();
  const sheet = page.getByRole('dialog', { name: 'New item' });
  await expect(sheet).toBeVisible();
  await sheet.getByLabel('Repository').selectOption('archon');
  await sheet.getByLabel('Title').fill(title);
  await sheet.getByRole('button', { name: 'Queue item' }).click();
  await expect(sheet.getByRole('alert')).toContainText('brief');
  await sheet.getByLabel('Brief').fill('Created through the new-item sheet.');
  await sheet.getByRole('radio', { name: 'High' }).click();
  await sheet.getByRole('button', { name: 'Queue item' }).click();
  await expect(sheet).toHaveCount(0);
  await expect(page.getByRole('link', { name: new RegExp(`^${title}`) })).toBeVisible();

  const summaries = await fixture.archond.request({ cmd: 'list_item_summaries', states: [], repo: null, pipeline: null, tag: null, limit: null });
  const made = summaries.data.find((s) => s.title === title);
  expect(made?.priority).toBe('high');
}

export async function cancelItem(page, fixture) {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: unique('Cancel me'), brief: 'Here to be cancelled.' });
  await openApp(page, fixture, `#/item/${item.id}`);
  await expect(page.getByRole('heading', { name: item.title, level: 1 })).toBeVisible();
  await page.getByRole('group', { name: 'Item actions' }).getByRole('button', { name: 'Cancel' }).click();
  const confirm = page.getByRole('dialog', { name: 'Cancel this item?' });
  await confirm.getByRole('button', { name: 'Cancel item' }).click();
  await expect(confirm).toHaveCount(0);
  await expect(page.locator('.item-hero')).toContainText('Cancelled');

  const detail = await fixture.archond.request({ cmd: 'get_item', item: item.id });
  expect(detail.data.item.state.state).toBe('cancelled');
}

// A stage settling posts a notification naming the item, not an item change.
export async function followItemLive(page, fixture) {
  await ensureRepo(fixture, 'archon');
  const item = await enqueue(fixture.archond, { repo: 'archon', title: unique('Follow me'), brief: 'Reloads on its own news.' });
  const loads = [];
  page.on('websocket', (ws) => ws.on('framesent', ({ payload }) => {
    const frame = typeof payload === 'string' ? JSON.parse(payload) : null;
    if (frame?.cmd === 'get_item' && frame.item === item.id) loads.push(Date.now());
  }));
  await openApp(page, fixture, `#/item/${item.id}`);
  await expect(page.getByRole('heading', { name: item.title, level: 1 })).toBeVisible();
  const before = loads.length;
  expect(before).toBeGreaterThan(0);
  await notify(fixture.archond, 'progress', `Stage review settled for ${item.id}.`, item.id);
  await expect.poll(() => loads.length, { timeout: 5_000 }).toBeGreaterThan(before);
}

export async function globalPauseResume(page, fixture) {
  await openApp(page, fixture, '#/overview');
  const toggle = page.getByRole('switch', { name: 'Pause new work' });
  await expect(toggle).not.toBeChecked();
  await toggle.click();
  await expect(page.locator('.hero-label')).toHaveText('Paused');
  expect((await fixture.archond.request({ cmd: 'status' })).data.pause.global).toBe(true);
  await toggle.click();
  await expect(page.locator('.hero-label')).toHaveText('Running');
  expect((await fixture.archond.request({ cmd: 'status' })).data.pause.global).toBe(false);
}

export const FLOWS = { pair, viewQueueAndItem, chat, answerQuestion, enqueueViaSheet, cancelItem, followItemLive, globalPauseResume };
