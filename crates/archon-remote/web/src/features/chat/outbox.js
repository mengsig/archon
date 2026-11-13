import { entryKey, stripRelayMark } from '../../core/conversation.js';

// Chat lines sent from here, until the orchestrator reads them and the
// conversation shows them. The outcome of each send (offline, refused) stays
// beside its operator entry.
export function addPending(store, text) {
  const entry = { localId: `local-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`, text, status: 'sending', error: null };
  store.set({ outbox: [...store.get().outbox, entry] });
  return entry.localId;
}

export function settlePending(store, localId, status, error = null) {
  if (!['sent', 'offline', 'failed'].includes(status)) throw new RangeError(`not a send outcome: ${status}`);
  const { outbox, chatNotes } = store.get();
  const entry = outbox.find((e) => e.localId === localId);
  if (entry) {
    store.set({ outbox: outbox.map((e) => (e.localId === localId ? { ...e, status, error } : e)) });
    return;
  }
  // The echo came first: the note goes on the conversation entry it became.
  const key = Object.keys(chatNotes).find((k) => chatNotes[k].localId === localId);
  if (key) store.set({ chatNotes: { ...chatNotes, [key]: { localId, status, error } } });
}

export function removePending(store, localId) {
  store.set({ outbox: store.get().outbox.filter((e) => e.localId !== localId) });
}

// Fresh operator entries replace the oldest pending send with the same text.
export function matchEchoes(store, fresh) {
  if (!Array.isArray(fresh)) throw new TypeError('matchEchoes needs the fresh conversation entries');
  let { outbox, chatNotes } = store.get();
  let changed = false;
  for (const item of fresh) {
    if (item.kind !== 'operator') continue;
    const text = stripRelayMark(String(item.text)).trim();
    const at = outbox.findIndex((e) => e.text.trim() === text);
    if (at < 0) continue;
    const entry = outbox[at];
    outbox = outbox.filter((_, i) => i !== at);
    // An echo proves the line was recorded, even when its send looked failed.
    const status = entry.status === 'failed' ? 'sent' : entry.status;
    chatNotes = { ...chatNotes, [entryKey(item)]: { localId: entry.localId, status, error: status === 'sent' ? null : entry.error } };
    changed = true;
  }
  if (changed) store.set({ outbox, chatNotes });
}
