import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { isShipped, shippedFiles } from '../../dev/shipped.js';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const MAX_SHIPPED_BYTES = 1024 * 1024;

test('the shipped rule keeps the app and drops tooling', () => {
  for (const path of ['index.html', 'sw.js', 'src/core/transport.js', 'styles/base.css', 'icons/icon.svg']) assert.ok(isShipped(path), path);
  for (const path of ['dev/server.js', 'tests/flows.js', 'scripts/e2e.js', 'node_modules/ws/index.js', 'package.json',
    'package-lock.json', 'README.md', 'playwright.config.js', 'src/x.test.js', 'src/x.spec.js', '.gitignore', 'src/.hidden.js']) {
    assert.ok(!isShipped(path), path);
  }
});

test('what ships stays well under a megabyte', () => {
  const total = shippedFiles(ROOT).reduce((sum, rel) => sum + statSync(join(ROOT, rel)).size, 0);
  assert.ok(total > 0 && total < MAX_SHIPPED_BYTES, `shipped ${total} bytes`);
});

test('the service worker precaches exactly the shipped shell', () => {
  const source = readFileSync(join(ROOT, 'sw.js'), 'utf8');
  const listed = [...source.match(/const SHELL = \[([^\]]*)\]/)[1].matchAll(/'([^']+)'/g)].map((m) => m[1]);
  const expected = shippedFiles(ROOT).filter((rel) => rel !== 'sw.js' && rel !== 'index.html').map((rel) => `/${rel}`);
  assert.deepEqual([...listed].filter((p) => p !== '/').sort(), expected.sort());
  assert.ok(listed.includes('/'), 'the page itself is cached under /');
  assert.ok(!listed.some((p) => p.startsWith('/api/') || p === '/ws'), 'the API is never cached');
});

test('the page uses no inline script or style, which the CSP would block', () => {
  const html = readFileSync(join(ROOT, 'index.html'), 'utf8');
  assert.doesNotMatch(html, /<script(?![^>]*\bsrc=)[^>]*>/i);
  assert.doesNotMatch(html, /<style|\sstyle=|\son[a-z]+=/i);
});
