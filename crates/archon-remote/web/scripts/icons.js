// Renders icons/icon.svg into the PNG sizes the manifest and iOS want.
//   node scripts/icons.js
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import assert from 'node:assert/strict';
import { chromium } from '@playwright/test';

const ICONS = resolve(dirname(fileURLToPath(import.meta.url)), '..', 'icons');
const SIZES = [['icon-192.png', 192], ['icon-512.png', 512], ['apple-touch-icon.png', 180]];

const svg = readFileSync(join(ICONS, 'icon.svg'), 'utf8');
assert.ok(svg.includes('viewBox="0 0 512 512"'), 'icon.svg should be a 512 square');
const browser = await chromium.launch();
try {
  for (const [name, size] of SIZES) {
    const page = await browser.newPage({ viewport: { width: size, height: size }, deviceScaleFactor: 1 });
    await page.setContent(`<html><body style="margin:0">${svg.replace('<svg ', `<svg width="${size}" height="${size}" `)}</body></html>`);
    await page.screenshot({ path: join(ICONS, name), clip: { x: 0, y: 0, width: size, height: size } });
    await page.close();
    console.log(`wrote icons/${name}`);
  }
} finally {
  await browser.close();
}
