// Notes typed while offline stay in localStorage and are sent again, in order,
// with the same client_key, so the daemon never records one twice.
const PREFIX = 'archon.design.outbox.';
// A refusal that will never succeed: the entry is dropped and the operator told.
const FINAL = new Set(['gone', 'finishing', 'read_only', 'forbidden', 'invalid']);

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
      const entry = { ...fields, client_key: fields.client_key ?? newKey() };
      write([...read().filter((e) => e.client_key !== entry.client_key), entry]);
      return entry;
    },
    remove(clientKey) {
      write(read().filter((e) => e.client_key !== clientKey));
    },
    pending: read,
    // `send(fields)` is design_note_add; the session is the current one.
    async flush(send, session, { onSent = () => {}, onRefused = () => {} } = {}) {
      if (flushing) return;
      flushing = true;
      try {
        for (const entry of read()) {
          try {
            const note = await send({ ...entry, session });
            this.remove(entry.client_key);
            onSent(entry, note);
          } catch (err) {
            if (FINAL.has(err?.code)) {
              this.remove(entry.client_key);
              onRefused(entry, err);
              continue;
            }
            break; // still offline: keep this one and the rest, in order
          }
        }
      } finally {
        flushing = false;
      }
    },
  };
}
