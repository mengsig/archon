// Boot: take a pairing link's code, ask the host who we are, then show
// either pairing or the app.
import * as api from './core/api.js';
import { codeFromHash } from './core/pairing-code.js';
import { followSystemTheme } from './core/theme.js';
import { createApp } from './app.js';
import { mountShell } from './shell.js';
import { showPairing } from './features/pairing/pairing.js';
import { h } from './ui/dom.js';
import { errorNote } from './ui/components.js';

const root = document.getElementById('app');
let running = null;

followSystemTheme();
registerServiceWorker();
boot({ code: takeLinkCode() });

// The fragment never stays in the address bar or history once read.
function takeLinkCode() {
  const code = codeFromHash(location.hash);
  if (code !== null || location.pathname === '/pair') history.replaceState(null, '', '/');
  return code;
}

async function boot({ code = null, notice = null } = {}) {
  let session;
  try {
    session = await api.session();
  } catch (err) {
    renderBootError(err);
    return;
  }
  if (!session) {
    showPairing(root, { code, notice, onPaired: () => boot() });
    return;
  }
  startPaired(session);
}

function startPaired(session) {
  const app = createApp({
    session,
    onUnpaired: (notice) => {
      stopRunning();
      showPairing(root, { notice, onPaired: () => boot() });
    },
  });
  const unmount = mountShell(root, app, {
    onForget: () => {
      stopRunning();
      showPairing(root, { notice: 'This device was forgotten. Pair it again to reconnect.', onPaired: () => boot() });
    },
  });
  running = { app, unmount };
  app.start();
}

function stopRunning() {
  if (!running) return;
  running.app.stop();
  running.unmount();
  running = null;
}

function renderBootError(err) {
  root.replaceChildren(h('div', { class: 'boot-error' },
    h('h1', { class: 'screen-title', text: 'Archon is out of reach' }),
    errorNote(err, () => boot())));
}

function registerServiceWorker() {
  if (!('serviceWorker' in navigator)) return;
  navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch((err) => {
    console.warn('Archon works without offline support: the service worker did not register.', err);
  });
}
