// Caches the app shell so Archon opens without a network. Never caches
// the API or the live socket: those are always fresh or fail visibly.
const CACHE = 'archon-shell-v1';
const SHELL = [
  '/',
  '/manifest.webmanifest',
  '/icons/icon.svg',
  '/icons/icon-192.png',
  '/icons/icon-512.png',
  '/icons/apple-touch-icon.png',
  '/src/app.js',
  '/src/core/api.js',
  '/src/core/backoff.js',
  '/src/core/daemon.js',
  '/src/core/device-name.js',
  '/src/core/feed.js',
  '/src/core/format.js',
  '/src/core/pairing-code.js',
  '/src/core/router.js',
  '/src/core/store.js',
  '/src/core/theme.js',
  '/src/core/transport.js',
  '/src/features/chat/chat.js',
  '/src/features/chat/entries.js',
  '/src/features/chat/outbox.js',
  '/src/features/inbox/inbox.js',
  '/src/features/inbox/question-card.js',
  '/src/features/item/actions.js',
  '/src/features/item/artifacts.js',
  '/src/features/item/item.js',
  '/src/features/item/stages.js',
  '/src/features/new-item/new-item.js',
  '/src/features/overview/catalog.js',
  '/src/features/overview/overview.js',
  '/src/features/pairing/pairing.js',
  '/src/features/queue/item-row.js',
  '/src/features/queue/queue.js',
  '/src/features/settings/settings.js',
  '/src/main.js',
  '/src/shell.js',
  '/src/ui/components.js',
  '/src/ui/dom.js',
  '/src/ui/icons.js',
  '/src/ui/list.js',
  '/src/ui/motion.js',
  '/src/ui/sheet.js',
  '/src/ui/toast.js',
  '/src/version.js',
  '/styles/base.css',
  '/styles/components.css',
  '/styles/screens.css',
  '/styles/tokens.css',
];

self.addEventListener('install', (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener('activate', (event) => {
  event.waitUntil(caches.keys()
    .then((keys) => Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key))))
    .then(() => self.clients.claim()));
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  const url = new URL(request.url);
  if (request.method !== 'GET' || url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/api/') || url.pathname === '/ws') return;
  // Every navigation is the one page; keep the fragment-free root as its key.
  const key = request.mode === 'navigate' ? '/' : url.pathname;
  if (request.mode !== 'navigate' && !SHELL.includes(key)) return;
  event.respondWith(networkFirst(request, key));
});

// The network wins so a new release shows at once; the cache is the
// offline fallback, and a miss in both stays a visible network error.
async function networkFirst(request, key) {
  const cache = await caches.open(CACHE);
  try {
    const response = await fetch(request, { cache: 'no-cache' });
    if (response.ok && response.type === 'basic') await cache.put(key, response.clone());
    return response;
  } catch (err) {
    const cached = await cache.match(key);
    if (cached) return cached;
    throw err;
  }
}
