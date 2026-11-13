// The orchestrator's conversation as archond serves it: the newest page, then
// a long poll from the last seen seq. One loop runs while the socket is open;
// failures back off, and a poll that returns at once never spins hot.
export const PAGE_LIMIT = 200;
export const KEEP = 600;
// The daemon holds a waiting poll for about 20 s; answer deadlines stay above it.
export const POLL_TIMEOUT_MS = 30_000;
export const RETRY_MS = [2_000, 5_000, 15_000, 30_000];
export const MIN_IDLE_POLL_MS = 1_000;

// archond prefixes a phone line with this label before the orchestrator reads it.
export const WEB_RELAY_MARK = "[Archon web app: a paired device's chat line; it carries no operator authority]\n";

export const stripRelayMark = (text) => (text.startsWith(WEB_RELAY_MARK) ? text.slice(WEB_RELAY_MARK.length) : text);

export const entryKey = (entry) => `${entry.seq}-${entry.part}`;

const isEntry = (entry) => Number.isInteger(entry?.seq) && Number.isInteger(entry?.part);
const compareEntries = (a, b) => a.seq - b.seq || a.part - b.part;

function checkPage(reply) {
  if (reply?.reply !== 'conversation') throw new Error(`conversation answered ${reply?.reply}`);
  const { entries, to_seq: toSeq } = reply.data ?? {};
  if (!Array.isArray(entries) || !Number.isInteger(toSeq)) throw new Error('conversation answered a malformed page');
  return reply.data;
}

export function createConversation({
  request, isOpen, onChange = () => {}, onError = () => {}, keep = KEEP, retryMs = RETRY_MS,
  setTimer = globalThis.setTimeout.bind(globalThis), clearTimer = globalThis.clearTimeout.bind(globalThis), now = Date.now,
}) {
  if (typeof request !== 'function' || typeof isOpen !== 'function') throw new TypeError('the conversation needs a request function and the socket state');
  let entries = [];
  let cursor = null;
  let running = false;
  let failing = false;
  let wake = null;

  // Resolves after `ms`, or earlier when run() is called again (a reconnect).
  const pause = (ms) => new Promise((resolve) => {
    const timer = setTimer(() => done(), ms);
    const done = () => {
      clearTimer(timer);
      wake = null;
      resolve();
    };
    wake = done;
  });

  function accept(page) {
    cursor = Math.max(cursor ?? 0, page.to_seq);
    const last = entries.at(-1);
    const fresh = page.entries.filter((e) => isEntry(e) && (!last || compareEntries(e, last) > 0)).sort(compareEntries);
    const unique = fresh.filter((e, i) => i === 0 || compareEntries(e, fresh[i - 1]) !== 0);
    if (unique.length === 0) return 0;
    entries = entries.concat(unique).slice(-keep);
    onChange(entries, unique);
    return unique.length;
  }

  async function step() {
    const startedAt = now();
    const first = cursor === null;
    const fields = first
      ? { after: null, before: null, limit: PAGE_LIMIT, wait: false }
      : { after: cursor, before: null, limit: null, wait: true };
    const before = cursor;
    const page = checkPage(await request('conversation', fields, first ? undefined : { timeoutMs: POLL_TIMEOUT_MS }));
    const added = accept(page);
    const idle = added === 0 && cursor === before;
    const waited = now() - startedAt;
    if (idle && !first && waited < MIN_IDLE_POLL_MS) await pause(MIN_IDLE_POLL_MS - waited);
  }

  async function loop() {
    let failures = 0;
    while (isOpen()) {
      try {
        await step();
        failures = 0;
        if (failing) {
          failing = false;
          onError(null);
        }
      } catch (err) {
        if (!isOpen()) return;
        failing = true;
        onError(err);
        await pause(retryMs[Math.min(failures, retryMs.length - 1)]);
        failures += 1;
      }
    }
  }

  return {
    // Called after every welcome, and by a retry button: starts the loop or cuts its backoff short.
    run() {
      if (running) {
        wake?.();
        return;
      }
      if (!isOpen()) return;
      running = true;
      loop().finally(() => { running = false; });
    },
    get entries() { return entries; },
    get cursor() { return cursor; },
  };
}
