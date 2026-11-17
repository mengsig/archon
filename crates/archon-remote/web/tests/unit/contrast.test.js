import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
const source = readFileSync(new URL('../../styles/tokens.css', import.meta.url), 'utf8');
const blocks = [...source.matchAll(/:root[^\{]*\{([^}]+)\}/g)].map((match) => Object.fromEntries(
  [...match[1].matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6});/g)].map((token) => [token[1], token[2]])));
const luminance = (hex) => {
  const rgb = hex.slice(1).match(/../g).map((value) => parseInt(value, 16) / 255)
    .map((value) => value <= .04045 ? value / 12.92 : ((value + .055) / 1.055) ** 2.4);
  return .2126 * rgb[0] + .7152 * rgb[1] + .0722 * rgb[2];
};
const contrast = (a, b) => (Math.max(luminance(a), luminance(b)) + .05) / (Math.min(luminance(a), luminance(b)) + .05);
for (const [index, theme] of [blocks[0], { ...blocks[0], ...blocks[1] }].entries()) {
  test(`${index ? 'dark' : 'light'} theme text and state tokens meet AA`, () => {
    for (const foreground of ['text', 'text-2', 'text-3', 'accent']) {
      for (const background of ['bg', 'surface', 'surface-2']) {
        assert.ok(contrast(theme[foreground], theme[background]) >= 4.5, `${foreground} on ${background}: ${contrast(theme[foreground], theme[background])}`);
      }
    }
    for (const tone of ['green', 'amber', 'red', 'accent']) {
      assert.ok(contrast(theme[tone], theme[`${tone}-soft`]) >= 4.5, `${tone} pill`);
    }
    assert.ok(contrast(theme['on-primary'], theme.primary) >= 4.5);
    assert.ok(contrast(theme['on-accent'], theme.accent) >= 4.5);
    assert.ok(contrast(theme['on-danger'], theme.red) >= 4.5);
    assert.ok(contrast('#ffffff', theme['switch-off']) >= 3, 'off switch thumb remains identifiable');
  });
}
