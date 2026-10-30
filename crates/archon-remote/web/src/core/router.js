// Hash routes, so any path the server answers with index.html works.
const ROUTES = [
  { name: 'queue', pattern: /^#\/(?:queue)?$/, depth: 0, tab: 'queue' },
  { name: 'item', pattern: /^#\/item\/([A-Za-z0-9_]{1,64})$/, depth: 1, tab: 'queue' },
  { name: 'inbox', pattern: /^#\/inbox$/, depth: 0, tab: 'inbox' },
  { name: 'chat', pattern: /^#\/chat$/, depth: 0, tab: 'chat' },
  { name: 'overview', pattern: /^#\/overview$/, depth: 0, tab: 'overview' },
  { name: 'settings', pattern: /^#\/settings$/, depth: 1, tab: 'overview' },
];

export function parseRoute(hash) {
  const value = hash || '#/';
  for (const route of ROUTES) {
    const match = route.pattern.exec(value);
    if (match) return { name: route.name, depth: route.depth, tab: route.tab, param: match[1] ?? null };
  }
  return { name: 'queue', depth: 0, tab: 'queue', param: null };
}

// How a view enters: pushed deeper, popped back, or swapped between tabs.
export function transitionBetween(from, to) {
  if (!from) return 'none';
  if (to.depth > from.depth) return 'push';
  if (to.depth < from.depth) return 'pop';
  return from.name === to.name ? 'none' : 'fade';
}

export function createRouter(onRoute) {
  let current = null;
  const apply = () => {
    const next = parseRoute(location.hash);
    // popstate and hashchange both fire for one traversal; render it once.
    if (current && current.name === next.name && current.param === next.param) return;
    const transition = transitionBetween(current, next);
    current = next;
    onRoute(next, transition);
  };
  return {
    start() {
      window.addEventListener('hashchange', apply);
      window.addEventListener('popstate', apply);
      apply();
    },
    stop() {
      window.removeEventListener('hashchange', apply);
      window.removeEventListener('popstate', apply);
    },
    // In-app navigation marks its history entry, so `back` knows it may pop it.
    go(hash) {
      if (!hash.startsWith('#/')) throw new Error(`routes start with #/, got ${hash}`);
      if (location.hash === hash) return;
      history.pushState({ archon: true }, '', hash);
      apply();
    },
    back(fallback) {
      if (history.state?.archon) {
        history.back();
        return;
      }
      history.replaceState(null, '', fallback);
      apply();
    },
    get current() { return current; },
  };
}
