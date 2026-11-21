// Caches the app shell so Archon opens without a network. Never caches
// the API or the live socket: those are always fresh or fail visibly.
const CACHE = 'archon-shell-v8';
// CacheStorage is shared by the whole origin, so only caches with this prefix are ours.
const CACHE_PREFIX = 'archon-shell-';
// The worker sits at the app's root, "" or the mount prefix.
const BASE = new URL('.', self.location.href).pathname.replace(/\/$/, '');
const SHELL = [
  `${BASE}/`,
  `${BASE}/icons/apple-touch-icon.png`,
  `${BASE}/icons/icon-192.png`,
  `${BASE}/icons/icon-512.png`,
  `${BASE}/icons/icon.svg`,
  `${BASE}/manifest.webmanifest`,
  `${BASE}/src/app.js`,
  `${BASE}/src/boot-watch.js`,
  `${BASE}/src/core/api.js`,
  `${BASE}/src/core/backoff.js`,
  `${BASE}/src/core/base.js`,
  `${BASE}/src/core/changes.js`,
  `${BASE}/src/core/conversation.js`,
  `${BASE}/src/core/daemon.js`,
  `${BASE}/src/core/design.js`,
  `${BASE}/src/core/device-name.js`,
  `${BASE}/src/core/feed.js`,
  `${BASE}/src/core/format.js`,
  `${BASE}/src/core/live-query.js`,
  `${BASE}/src/core/pairing-code.js`,
  `${BASE}/src/core/role.js`,
  `${BASE}/src/core/router.js`,
  `${BASE}/src/core/session-signal.js`,
  `${BASE}/src/core/session-watch.js`,
  `${BASE}/src/core/store.js`,
  `${BASE}/src/core/theme.js`,
  `${BASE}/src/core/transport.js`,
  `${BASE}/src/features/chat/chat.js`,
  `${BASE}/src/features/chat/entries.js`,
  `${BASE}/src/features/chat/outbox.js`,
  `${BASE}/src/features/commands/commands.js`,
  `${BASE}/src/features/design/chrome.js`,
  `${BASE}/src/features/design/compare.js`,
  `${BASE}/src/features/design/composer.js`,
  `${BASE}/src/features/design/details.js`,
  `${BASE}/src/features/design/finish.js`,
  `${BASE}/src/features/design/input.js`,
  `${BASE}/src/features/design/keymap.js`,
  `${BASE}/src/features/design/notes.js`,
  `${BASE}/src/features/design/outbox.js`,
  `${BASE}/src/features/design/overlay.js`,
  `${BASE}/src/features/design/scale.js`,
  `${BASE}/src/features/design/screen.js`,
  `${BASE}/src/features/design/sketch-model.js`,
  `${BASE}/src/features/design/sketch.js`,
  `${BASE}/src/features/design/state.js`,
  `${BASE}/src/features/design/stream.js`,
  `${BASE}/src/features/graph/data.js`,
  `${BASE}/src/features/graph/graph.js`,
  `${BASE}/src/features/graph/model.js`,
  `${BASE}/src/features/inbox/inbox.js`,
  `${BASE}/src/features/inbox/question-card.js`,
  `${BASE}/src/features/item/actions.js`,
  `${BASE}/src/features/item/artifacts.js`,
  `${BASE}/src/features/item/graph-models.js`,
  `${BASE}/src/features/item/graphs.js`,
  `${BASE}/src/features/item/item.js`,
  `${BASE}/src/features/item/relationships.js`,
  `${BASE}/src/features/item/stages.js`,
  `${BASE}/src/features/new-item/new-item.js`,
  `${BASE}/src/features/overview/catalog.js`,
  `${BASE}/src/features/overview/dashboard.js`,
  `${BASE}/src/features/overview/overview.js`,
  `${BASE}/src/features/overview/spend.js`,
  `${BASE}/src/features/pairing/pairing.js`,
  `${BASE}/src/features/queue/item-row.js`,
  `${BASE}/src/features/queue/queue.js`,
  `${BASE}/src/features/settings/settings.js`,
  `${BASE}/src/main.js`,
  `${BASE}/src/shell.js`,
  `${BASE}/src/ui/components.js`,
  `${BASE}/src/ui/dom.js`,
  `${BASE}/src/ui/entities.js`,
  `${BASE}/src/ui/graph/camera.js`,
  `${BASE}/src/ui/graph/controller.js`,
  `${BASE}/src/ui/graph/graph.js`,
  `${BASE}/src/ui/graph/interactions.js`,
  `${BASE}/src/ui/graph/layout.js`,
  `${BASE}/src/ui/graph/render.js`,
  `${BASE}/src/ui/icons.js`,
  `${BASE}/src/ui/list.js`,
  `${BASE}/src/ui/markdown-parse.js`,
  `${BASE}/src/ui/markdown.js`,
  `${BASE}/src/ui/motion.js`,
  `${BASE}/src/ui/sheet.js`,
  `${BASE}/src/ui/toast.js`,
  `${BASE}/src/version.js`,
  `${BASE}/styles/base.css`,
  `${BASE}/styles/components.css`,
  `${BASE}/styles/design.css`,
  `${BASE}/styles/graph.css`,
  `${BASE}/styles/layout.css`,
  `${BASE}/styles/screens.css`,
  `${BASE}/styles/tokens.css`,
];

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(caches.keys()
    .then((keys) => Promise.all(keys.filter((key) => key.startsWith(CACHE_PREFIX) && key !== CACHE).map((key) => caches.delete(key))))
    .then(() => self.clients.claim()));
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== 'GET' || url.origin !== self.location.origin) return;
  if (url.pathname.startsWith(`${BASE}/api/`) || url.pathname === `${BASE}/ws`) return;
  // Every navigation is the one page; keep the fragment-free root as its key.
  const key = request.mode === 'navigate' ? `${BASE}/` : url.pathname;
  if (request.mode !== 'navigate' && !SHELL.includes(key)) return;
  event.respondWith(networkFirst(request, key));
});

// The network wins so a new release shows at once; the cache is the
// fallback offline or while the host is busy (503), and a miss in both
// stays visible: the busy page, or a network error.
async function networkFirst(request, key) {
  const cache = await caches.open(CACHE);
  try {
    const response = await fetch(request, { cache: 'no-cache' });
    if (response.ok && response.type === 'basic') await cache.put(key, response.clone());
    if (response.status === 503) return (await cache.match(key)) ?? response;
    return response;
  } catch (err) {
    const cached = await cache.match(key);
    if (cached) return cached;
    throw err;
  }
}
