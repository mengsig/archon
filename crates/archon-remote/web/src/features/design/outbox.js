// Replay keeps both the original session and the client key.
const PREFIX = 'archon.design.outbox.';
const TRANSIENT = new Set(['disconnected', 'timeout', 'unavailable', 'recovering', 'unauthorized', 'finishing']);

const hasSession = (id) => typeof id === 'string' && id.trim().length > 0;
export const retryableNoteError = (error) => TRANSIENT.has(error?.code);

export function blockedDraftReason(entry, session, state) {
  if (!hasSession(entry.session)) return 'The original design session is unknown.';
  if (hasSession(session) && entry.session !== session) return 'This draft belongs to an earlier design session.';
  if (entry.session_ended || state === 'ended' || state === 'confirmed') return 'The original design session has ended.';
  if (!hasSession(session)) return 'Waiting to verify the original design session.';
  return null;
}

export function newClientKey(random = () => crypto.getRandomValues(new Uint8Array(12))) {
  return `web-${Array.from(random(), (b) => b.toString(16).padStart(2, '0')).join('')}`;
}

export function createNoteOutbox({ storage = globalThis.localStorage, item, newKey = newClientKey }) {
  const key = `${PREFIX}${item}`;
  const read = () => {
    try {
      const list = JSON.parse(storage?.getItem(key) ?? '[]');
      return Array.isArray(list) ? list.filter((e) => typeof e?.client_key === 'string') : [];
    } catch {
      return [];
    }
  };
  const write = (list) => {
    try {
      if (list.length) storage?.setItem(key, JSON.stringify(list));
      else storage?.removeItem(key);
    } catch { /* a full or blocked store only loses the offline copy */ }
  };
  let flushing = false;
  return {
    // Remember a note before the first attempt, so a dropped connection loses nothing.
    add(fields) {
      if (!hasSession(fields.session) || typeof fields.text !== 'string' || !fields.text.trim()) {
        throw new TypeError('A design draft needs a session and note text');
      }
      const entry = { ...fields, client_key: fields.client_key ?? newKey() };
      if (typeof entry.client_key !== 'string' || !entry.client_key) throw new TypeError('A design draft needs a client key');
      write([...read().filter((e) => e.client_key !== entry.client_key), entry]);
      return entry;
    },
    remove(clientKey) {
      write(read().filter((e) => e.client_key !== clientKey));
    },
    markEnded(clientKey) {
      write(read().map((entry) => entry.client_key === clientKey ? { ...entry, session_ended: true } : entry));
    },
    pending: read,
    async flush(send, session, { canSend = () => true, onSent = () => {}, onRefused = () => {} } = {}) {
      if (flushing || !hasSession(session)) return;
      flushing = true;
      try {
        for (const entry of read()) {
          if (entry.session !== session || entry.session_ended) continue;
          if (!canSend()) break;
          let note;
          try {
            note = await send(entry);
          } catch (err) {
            if (retryableNoteError(err)) break;
            if (err?.code === 'gone') this.markEnded(entry.client_key);
            else this.remove(entry.client_key);
            onRefused(entry, err);
            if (err?.code === 'gone') break;
            continue;
          }
          this.remove(entry.client_key);
          onSent(entry, note);
        }
      } finally {
        flushing = false;
      }
    },
  };
}
