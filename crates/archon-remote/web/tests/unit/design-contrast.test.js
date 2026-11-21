import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const source = readFileSync(new URL('../../styles/tokens.css', import.meta.url), 'utf8');
const blocks = [...source.matchAll(/:root[^\{]*\{([^}]+)\}/g)].map((m) => Object.fromEntries(
  [...m[1].matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6});/g)].map((t) => [t[1], t[2]])));
const lum = (hex) => {
  const [r, g, b] = hex.slice(1).match(/../g).map((v) => parseInt(v, 16) / 255).map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const ratio = (a, b) => (Math.max(lum(a), lum(b)) + 0.05) / (Math.min(lum(a), lum(b)) + 0.05);

for (const [name, theme] of [['light', blocks[0]], ['dark', { ...blocks[0], ...blocks[1] }]]) {
  test(`${name}: design screen text pairs meet 4.5:1`, () => {
    // Status chips and notes sit on the desk and on cards (surface-2 / surface).
    for (const tone of ['green', 'amber', 'red', 'accent']) {
      for (const bg of ['surface', 'surface-2']) assert.ok(ratio(theme[tone], theme[bg]) >= 4.5, `${tone} on ${bg}`);
    }
    for (const fg of ['text', 'text-2']) for (const bg of ['surface', 'surface-2', 'accent-soft', 'amber-soft', 'red-soft']) {
      assert.ok(ratio(theme[fg], theme[bg]) >= 4.5, `${fg} on ${bg}`);
    }
    assert.ok(ratio(theme.accent, theme['accent-soft']) >= 4.5, 'selected mode button');
    assert.ok(ratio(theme.text, theme.bg) >= 4.5, 'capture chip: bg on text');
    assert.ok(ratio(theme['on-accent'], theme.accent) >= 4.5, 'primary button');
  });
  test(`${name}: the overlay is visible over any app (3:1) and its label reads (4.5:1)`, () => {
    assert.ok(ratio(theme['frame-accent'], theme['frame-halo']) >= 4.5, 'label and pin text on the outline colour');
    assert.ok(ratio(theme['frame-accent'], theme['frame-halo']) >= 3, 'outline against its halo');
  });
  test(`${name}: UI edges against the desk meet 3:1`, () => {
    assert.ok(ratio(theme.accent, theme['surface-2']) >= 3, 'focus ring on the desk');
    assert.ok(ratio(theme['switch-off'], theme.surface) >= 3, 'switch off state');
  });
}
