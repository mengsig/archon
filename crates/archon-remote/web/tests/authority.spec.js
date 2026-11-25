// A phone paired as an operator does from the app what the operator does
// from the TUI; one paired as an observer is refused, with the reason.
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
  // Aether is down here: the chat shows archond's own answer under the line, in plain words.
  await expect(setLine.getByRole('status')).toHaveText(/^Archon: Directive dir_\w+ in force: docs fixes only today\. Applied\.$/);
  // The sidebar names the running daemon's version, not one built into the app.
  const { data: status } = await fixture.archond.request({ cmd: 'status' });
  await expect(page.locator('.nav-version')).toHaveText(`Archon ${status.daemon_version}`);

  const clearLine = await send(page, '/archon apply {"cmd":"clear_directive","directive":null}');
  await expect.poll(() => directives(fixture)).toEqual([]);
  await expect(clearLine.getByRole('status')).toHaveText('Archon: All directives cleared (1). Applied.');
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

// The app shows an observer no control, so force a change over its own
// WebSocket session: the daemon must still refuse it with the reason.
async function forceRequest(page, protocolVersion, request) {
  return page.evaluate(({ protocolVersion, request }) => new Promise((resolve, reject) => {
    const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`, 'archon');
    const frames = [];
    const timer = setTimeout(() => reject(new Error(`no reply within 10 s: ${frames.join(' | ')}`)), 10_000);
    socket.onclose = ({ code, reason }) => reject(new Error(`closed ${code} ${reason}: ${frames.join(' | ')}`));
    socket.onopen = () => socket.send(JSON.stringify({ req: 1, cmd: 'hello', protocol_version: protocolVersion, client: 'archon-web', client_version: '1.0.0' }));
    socket.onmessage = ({ data }) => {
      const frame = JSON.parse(data);
      frames.push(String(data).slice(0, 200));
      if (frame.frame !== 'reply') return;
      if (frame.id === 1) return socket.send(JSON.stringify({ req: 2, ...request }));
      clearTimeout(timer);
      socket.close();
      resolve(frame.result);
    };
  }), { protocolVersion, request });
}

test('observer: no change control shows, and a forced change is refused with the reason', async ({ browser, fixture }) => {
  const context = await browser.newContext();
  const page = await context.newPage();
  const problems = guardConsole(page);
  try {
    await pairAs(page, fixture, 'observer');
    await expect(page.getByText('This device is an observer: it reads the conversation but cannot send to it.')).toBeVisible();
    await expect(page.getByRole('textbox', { name: 'Message' })).toHaveCount(0);
    const result = await forceRequest(page, fixture.archond.protocolVersion, { cmd: 'set_directive', text: 'observer says', expires_in_hours: null });
    expect(JSON.stringify(result.Err)).toContain('set_directive changes Archon. This is an observer device: it can only read.');
    expect(await directives(fixture)).toEqual([]);
  } finally {
    await context.close();
  }
  expect(problems, 'the page logged errors').toEqual([]);
});
