// How many connections the app holds against archond's real listener, and
// what a browser sees when every connection slot is taken.
import { readFileSync } from 'node:fs';
import { createConnection } from 'node:net';
import { test as base, expect } from '@playwright/test';
import { openApp } from './flows.js';
import { startListenerFixture } from './support/listener.js';

const CAP = 8;
const TABS = 3;
const CANT_CONNECT = /Can[’']t connect to Archon/;

const test = base.extend({
  fixture: [async ({}, use) => {
    const fixture = await startListenerFixture({ maxConnections: CAP });
    try {
      await use(fixture);
    } finally {
      await fixture.stop();
    }
  }, { scope: 'worker', timeout: 120_000 }],
});

test.describe.configure({ mode: 'serial' });

// Established TCP connections whose local end is the listener's port.
function serverConnections(port) {
  const rows = readFileSync('/proc/net/tcp', 'utf8').trim().split('\n').slice(1);
  const hexPort = port.toString(16).toUpperCase().padStart(4, '0');
  return rows.map((row) => row.trim().split(/\s+/)).filter((cols) => cols[1].endsWith(`:${hexPort}`) && cols[3] === '01').length;
}

async function pairedState(browser, fixture) {
  const context = await browser.newContext();
  await openApp(await context.newPage(), fixture);
  const state = await context.storageState();
  await context.close();
  return state;
}

test('several tabs open at once and settle to their sockets alone', async ({ browser, fixture }) => {
  const context = await browser.newContext({ storageState: await pairedState(browser, fixture) });
  const pages = await Promise.all(Array.from({ length: TABS }, () => context.newPage()));
  await Promise.all(pages.map((page) => page.goto(`${fixture.baseURL}/#/overview`)));
  for (const page of pages) await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible();
  // Idle keep-alive connections close after the listener's 3 s head timeout.
  await expect.poll(() => serverConnections(fixture.listeningPort), { timeout: 10_000, intervals: [500] })
    .toBeLessThanOrEqual(TABS * 2);
  for (const page of pages) await expect(page.locator('.conn-banner')).toBeHidden();
  await context.close();
});

test('a browser beyond the cap is told it cannot connect, then gets in', async ({ browser, fixture }) => {
  const context = await browser.newContext({ storageState: await pairedState(browser, fixture) });
  const page = await context.newPage();
  const held = [];
  try {
    for (let n = 0; n < CAP; n += 1) {
      const socket = createConnection({ host: '127.0.0.1', port: fixture.listeningPort });
      held.push(socket);
      await new Promise((resolve, reject) => { socket.once('connect', resolve); socket.once('error', reject); });
    }
    await expect.poll(() => serverConnections(fixture.listeningPort)).toBeGreaterThanOrEqual(CAP);
    const response = await page.goto(`${fixture.baseURL}/#/overview`);
    expect(response.status()).toBe(503);
    // The first wait is 2 s plus up to half again of jitter, and the refresh waits as long.
    const wait = response.headers()['retry-after'];
    expect(['2', '3']).toContain(wait);
    await expect(page.locator('meta[http-equiv="refresh"]')).toHaveAttribute('content', `${wait}; url=./?busy=1`);
    await expect(page.getByRole('heading', { name: CANT_CONNECT })).toBeVisible();
  } finally {
    for (const socket of held) socket.destroy();
  }
  // The busy page reloads itself, and with slots free the app boots without the retry count.
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible({ timeout: 15_000 });
  expect(new URL(page.url()).searchParams.has('busy'), page.url()).toBe(false);
  await context.close();
});

test('a busy session check shows the retry screen with the host’s wait', async ({ browser, fixture }) => {
  const context = await browser.newContext({ storageState: await pairedState(browser, fixture) });
  const page = await context.newPage();
  let refusals = 1;
  await page.route('**/api/session', (route) => (refusals-- > 0
    ? route.fulfill({ status: 503, headers: { 'Retry-After': '2' }, contentType: 'application/json',
      body: JSON.stringify({ error: { code: 'busy', message: 'Archon is busy: every connection is in use. Retrying shortly.' } }) })
    : route.continue()));
  await page.goto(`${fixture.baseURL}/#/overview`);
  await expect(page.getByRole('heading', { name: CANT_CONNECT })).toBeVisible();
  await expect(page.getByRole('status').filter({ hasText: 'Archon is busy' })).toContainText(/Retrying in [23] s/);
  await expect(page.getByRole('navigation', { name: 'Sections' })).toBeVisible({ timeout: 10_000 });
  await context.close();
});
