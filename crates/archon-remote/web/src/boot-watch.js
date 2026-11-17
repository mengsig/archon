// A classic script that runs before the modules: when the host refuses or drops
// the connections the app loads over, it says so and reloads with a capped,
// jittered backoff (as core/backoff.js, which a classic script cannot import).
(() => {
  const BOOT_DEADLINE_MS = 8000;
  // Each attempt waits between half its ceiling and the whole of it.
  const CEILING_MS = [4000, 8000, 16000, 30000];
  const MAX_AUTO_RETRIES = 8;
  const ATTEMPTS_KEY = 'archon-boot-attempts';
  let failed = false;

  const attempts = () => Number.parseInt(sessionStorage.getItem(ATTEMPTS_KEY) ?? '0', 10) || 0;
  const booted = () => document.documentElement.dataset.booted === 'true';

  function fail() {
    if (failed || booted()) return;
    failed = true;
    const attempt = attempts();
    if (!Number.isInteger(attempt) || attempt < 0) throw new RangeError(`boot attempts must be a count, got ${attempt}`);
    sessionStorage.setItem(ATTEMPTS_KEY, String(attempt + 1));
    const auto = attempt < MAX_AUTO_RETRIES;
    showRetrying(auto ? jittered(CEILING_MS[Math.min(attempt, CEILING_MS.length - 1)]) : null);
  }

  function jittered(ceilingMs) {
    const r = Math.random();
    if (!(r >= 0 && r < 1) || !(ceilingMs > 0)) throw new RangeError(`no wait from ${r} and ${ceilingMs}`);
    return ceilingMs / 2 + (r * ceilingMs) / 2;
  }

  // `delayMs` null: automatic retries are used up, only the button retries.
  function showRetrying(delayMs) {
    const root = document.getElementById('app');
    if (!root) throw new Error('index.html has no #app');
    const element = (tag, props) => Object.assign(document.createElement(tag), props);
    const text = element('p', {});
    text.setAttribute('role', 'status');
    const retry = element('button', { type: 'button', className: 'button button-primary', textContent: 'Retry now', onclick: () => location.reload() });
    const box = element('div', { className: 'boot-error' });
    box.append(element('h1', { className: 'screen-title', textContent: 'Can’t connect to Archon' }), text, retry);
    root.replaceChildren(box);
    if (delayMs === null) {
      text.textContent = 'The host keeps refusing the connection. Check that Archon is running, then retry.';
      return;
    }
    const due = Date.now() + delayMs;
    // Counts down whole seconds and reloads at the jittered moment itself.
    const tick = () => {
      const leftMs = due - Date.now();
      if (leftMs <= 0) return location.reload();
      text.textContent = `The host refused or dropped the connection. Retrying in ${Math.ceil(leftMs / 1000)} s.`;
      setTimeout(tick, leftMs % 1000 || 1000);
    };
    tick();
  }

  // A module that fails to load, its own or any it imports, fires error at its script.
  window.addEventListener('error', (event) => {
    if (event.target instanceof HTMLScriptElement) fail();
  }, true);
  setTimeout(() => {
    if (booted()) sessionStorage.removeItem(ATTEMPTS_KEY);
    else fail();
  }, BOOT_DEADLINE_MS);
})();
