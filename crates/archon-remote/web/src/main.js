// Boot: take a pairing link's code, ask the host who we are, then show
// either pairing or the app.
import * as api from './core/api.js';
import { BASE } from './core/base.js';
import { codeFromHash } from './core/pairing-code.js';
import { followSystemTheme } from './core/theme.js';
import { createApp } from './app.js';
import { mountShell } from './shell.js';
import { showPairing } from './features/pairing/pairing.js';
import { h } from './ui/dom.js';
import { errorNote } from './ui/components.js';
import { backoffDelay } from './core/backoff.js';

const root = document.getElementById('app');
const MAX_AUTO_RETRIES = 8;
let running = null;
let retryTimer = null;

// The modules loaded: boot-watch.js stands down, and this file reports what fails next.
document.documentElement.dataset.booted = 'true';

followSystemTheme();
registerServiceWorker();
boot({ code: takeLinkCode() });

// The fragment never stays in the address bar or history once read.
function takeLinkCode() {
  const code = codeFromHash(location.hash);
  if (code !== null || location.pathname === `${BASE}/pair`) history.replaceState(null, '', `${BASE}/`);
  return code;
}

async function boot({ code = null, notice = null, attempt = 0 } = {}) {
  if (!Number.isInteger(attempt) || attempt < 0) throw new RangeError(`boot attempt must be a count, got ${attempt}`);
  clearInterval(retryTimer);
  let session;
  try {
    session = await api.session();
  } catch (err) {
    // 0: refused, dropped or silent; 503: every connection slot is taken for now.
    const retryable = err instanceof api.ApiError && (err.status === 0 || err.status === 503);
    if (retryable) renderRetrying(err, { code, notice, attempt });
    else renderBootError(err);
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

// The host refused, dropped or never answered: say so and retry on a capped backoff.
function renderRetrying(err, { code, notice, attempt }) {
  const status = h('p', { role: 'status' });
  const again = () => boot({ code, notice, attempt: attempt + 1 });
  root.replaceChildren(h('div', { class: 'boot-error' },
    h('h1', { class: 'screen-title', text: 'Can’t connect to Archon' }), status,
    h('button', { class: 'button button-primary', type: 'button', onclick: again }, 'Retry now')));
  if (attempt >= MAX_AUTO_RETRIES) {
    status.textContent = `${err.message} Check that Archon is running, then retry.`;
    return;
  }
  // A busy host names its own wait; the backoff covers one that said nothing.
  let left = Math.max(1, err.retryAfterSecs ?? Math.round(backoffDelay(attempt) / 1000));
  const tick = () => {
    status.textContent = `${err.message} Retrying in ${left} s.`;
    if (left-- <= 0) again();
  };
  tick();
  retryTimer = setInterval(tick, 1000);
}

function renderBootError(err) {
  root.replaceChildren(h('div', { class: 'boot-error' },
    h('h1', { class: 'screen-title', text: 'Archon is out of reach' }),
    errorNote(err, () => boot())));
}

function registerServiceWorker() {
  if (!('serviceWorker' in navigator)) return;
  navigator.serviceWorker.register(`${BASE}/sw.js`, { scope: `${BASE}/` }).catch((err) => {
    console.warn('Archon works without offline support: the service worker did not register.', err);
  });
}
