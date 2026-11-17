// A classic script that runs before the modules: when the host refuses or drops
// the connections the app loads over, it says so and reloads with a capped backoff.
(() => {
  const BOOT_DEADLINE_MS = 8000;
  const RETRY_MS = [2000, 4000, 8000, 15000, 30000];
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
    showRetrying(auto ? RETRY_MS[Math.min(attempt, RETRY_MS.length - 1)] : null);
  }

  // `delayMs` null: automatic retries are used up, only the button retries.
  function showRetrying(delayMs) {
    const root = document.getElementById('app');
    if (!root) throw new Error('index.html has no #app');
    const title = document.createElement('h1');
    title.className = 'screen-title';
    title.textContent = 'Can’t connect to Archon';
    const text = document.createElement('p');
    text.setAttribute('role', 'status');
    const retry = document.createElement('button');
    retry.type = 'button';
    retry.className = 'button button-primary';
    retry.textContent = 'Retry now';
    retry.addEventListener('click', () => location.reload());
    const box = document.createElement('div');
    box.className = 'boot-error';
    box.append(title, text, retry);
    root.replaceChildren(box);
    if (delayMs === null) {
      text.textContent = 'The host keeps refusing the connection. Check that Archon is running, then retry.';
      return;
    }
    let left = Math.round(delayMs / 1000);
    const tick = () => {
      text.textContent = `The host refused or dropped the connection. Retrying in ${left} s.`;
      if (left <= 0) location.reload();
      left -= 1;
    };
    tick();
    setInterval(tick, 1000);
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
