// Which files under web/ the host serves: the same rule the Rust embed
// follows, so the dev server and the tests see exactly what ships.
import { readdirSync } from 'node:fs';
import { join, relative, sep } from 'node:path';
import assert from 'node:assert/strict';

const EXCLUDED_DIRS = new Set(['dev', 'tests', 'scripts', 'node_modules', 'test-results', 'playwright-report']);
const EXCLUDED_FILES = new Set(['package.json', 'package-lock.json']);
const EXCLUDED_SUFFIXES = ['.test.js', '.spec.js', '.md', '.config.js'];

export function isShipped(relPath) {
  assert.ok(!relPath.startsWith('/') && !relPath.includes('\\'), `not a relative POSIX path: ${relPath}`);
  const parts = relPath.split('/');
  if (parts.some((part) => part === '' || part === '.' || part === '..' || part.startsWith('.'))) return false;
  if (parts.length > 1 && EXCLUDED_DIRS.has(parts[0])) return false;
  const name = parts.at(-1);
  if (EXCLUDED_FILES.has(name)) return false;
  return !EXCLUDED_SUFFIXES.some((suffix) => name.endsWith(suffix));
}

export function shippedFiles(root) {
  const found = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      const rel = relative(root, path).split(sep).join('/');
      if (entry.isDirectory()) {
        if (!entry.name.startsWith('.') && !EXCLUDED_DIRS.has(entry.name)) walk(path);
      } else if (entry.isFile() && isShipped(rel)) {
        found.push(rel);
      }
    }
  };
  walk(root);
  assert.ok(found.includes('index.html'), `web/index.html must ship (looked in ${root})`);
  return found.sort();
}
