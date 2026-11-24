import { entryKey, echoedLine, commandEchoId } from '../../core/conversation.js';

// Chat lines sent from here, until the orchestrator reads them and the
// conversation shows them. The outcome of each send (offline, refused) stays
// beside its operator entry. `after` is the last seq shown when the send began:
// only a later operator entry can be its echo.
export function addPending(store, text) {
  const { conversation } = store.get();
  if (!Array.isArray(conversation)) throw new TypeError('addPending needs the conversation in the store');
  const after = conversation.at(-1)?.seq ?? 0;
  const bytes = crypto.getRandomValues(new Uint8Array(16));
  bytes[6] = (bytes[6] & 0x0f) | 0x40;
  bytes[8] = (bytes[8] & 0x3f) | 0x80;
  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  const localId = `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  const entry = { localId, text, after, status: 'sending', error: null };
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

// Host commands are consumed by archond and echoed as posts. Only ordinary chat
// is relayed to the orchestrator and matched by transcript sequence and text.
const hostCommand = /^\/(answer|approve|reject|merge|archon|upgrade|update|watch|a)(?:\s|$)/i;
export function matchEchoes(store, fresh) {
  if (!Array.isArray(fresh)) throw new TypeError('matchEchoes needs the fresh conversation entries');
  let { outbox, chatNotes } = store.get();
  let changed = false;
  for (const item of fresh) {
    const line = echoedLine(item);
    if (line === null) continue;
    const text = line.trim();
    const id = commandEchoId(item);
    const at = id ? outbox.findIndex((e) => e.localId === id)
      : outbox.findIndex((e) => item.kind === 'operator' && !hostCommand.test(text) && item.seq > e.after && e.text.trim() === text);
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

export function commandOutcome(feed, pending) {
  if (!Array.isArray(feed) || typeof pending?.localId !== 'string' || !pending.localId) return null;
  const lines = [];
  for (const item of feed) {
    const entry = item.entry;
    if (entry?.entry !== 'system' || entry.command_id !== pending.localId || typeof entry.text !== 'string') continue;
    const line = entry.text.trim();
    let result = entry.command_result;
    if (result === undefined) {
      if (line.startsWith('Operator command: ')) result = line.slice('Operator command: '.length).trim();
      else if (line.startsWith('Operator command refused: ')) result = line;
    }
    if (typeof result !== 'string' || !result.trim()) {
      if (line) lines.push(line);
      continue;
    }
    const question = /\bQ(\d+)\b/.exec(/^Awaiting your confirmation/.test(result) ? result : '')?.[0] ?? null;
    return { lines: [...lines, result].filter(Boolean), question };
  }
  return null;
}
