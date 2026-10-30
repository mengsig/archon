// Fails a test on any console error, page error or CSP violation. The one
// expected error is the browser logging the 401 of an unpaired session check.
const EXPECTED = [
  (msg) => /status of 401/.test(msg.text()) && new URL(msg.location().url || 'http://x/').pathname === '/api/session',
];

export function guardConsole(page) {
  const problems = [];
  page.on('console', (msg) => {
    if (msg.type() !== 'error' && msg.type() !== 'warning') return;
    if (msg.type() === 'warning' && !/Content Security Policy|service worker/i.test(msg.text())) return;
    if (EXPECTED.some((expected) => expected(msg))) return;
    problems.push(`console ${msg.type()}: ${msg.text()} (${msg.location().url})`);
  });
  page.on('pageerror', (err) => problems.push(`page error: ${err.stack ?? err.message}`));
  return problems;
}
