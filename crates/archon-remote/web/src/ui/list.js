import { reducedMotion, animateMove } from './motion.js';

// Keyed list rendering: unchanged rows keep their nodes, new rows animate in.
export function reconcile(container, items, { key, signature, render }) {
  if (!(container instanceof Element)) throw new TypeError('reconcile needs a container element');
  const existing = new Map();
  for (const child of [...container.children]) {
    if (child.dataset.key) existing.set(child.dataset.key, child);
    else child.remove();
  }
  const positions = reducedMotion() ? new Map() : new Map([...existing].map(([key, node]) => [key, node.getBoundingClientRect()]));
  const firstRender = existing.size === 0;
  const seen = new Set();
  let cursor = container.firstElementChild;
  for (const item of items) {
    const k = String(key(item));
    if (seen.has(k)) throw new Error(`duplicate list key ${k}`);
    seen.add(k);
    const sig = signature(item);
    let node = existing.get(k);
    if (!node || node.dataset.sig !== sig) {
      const fresh = render(item);
      fresh.dataset.key = k;
      fresh.dataset.sig = sig;
      if (!node && !firstRender) fresh.classList.add('is-entering');
      if (node === cursor) cursor = fresh;
      if (node) node.replaceWith(fresh);
      node = fresh;
    }
    if (node !== cursor) container.insertBefore(node, cursor);
    else cursor = cursor.nextElementSibling;
  }
  // Removed rows settle immediately; retained rows move using one batched layout read.
  for (const [k, node] of existing) if (!seen.has(k) && !node.classList.contains('is-leaving')) node.remove();
  const moved = [...container.children].filter((node) => positions.has(node.dataset.key))
    .map((node) => ({ node, before: positions.get(node.dataset.key), after: node.getBoundingClientRect() }));
  for (const { node, before, after } of moved) {
    if (before.x !== after.x || before.y !== after.y) animateMove(node, [before.x - after.x, before.y - after.y], [0, 0]);
  }
}
