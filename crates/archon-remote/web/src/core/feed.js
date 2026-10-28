// The live feed: replayed history, then a subscription from the last seen
// seq, so a reconnect neither loses nor repeats an entry.
export const HISTORY_LIMIT = 200;
export const KEEP = 600;

export function createFeed({ request, keep = KEEP }) {
  if (typeof request !== 'function') throw new TypeError('the feed needs a request function');
  let lastSeq = null;
  let items = [];
  const listeners = new Set();

  function accept(list) {
    const floor = lastSeq ?? -1;
    const fresh = list.filter((item) => Number.isInteger(item?.seq) && item.seq > floor).sort((a, b) => a.seq - b.seq);
    if (fresh.length === 0) return;
    lastSeq = fresh.at(-1).seq;
    items = items.concat(fresh).slice(-keep);
    for (const listener of [...listeners]) listener(fresh, items);
  }

  return {
    // Called after every welcome: history once, then (re)subscribe.
    async sync() {
      if (lastSeq === null) {
        const history = await request('feed_history', { before: null, limit: HISTORY_LIMIT, verbose: false });
        if (history.reply !== 'feed' || !Array.isArray(history.data)) throw new Error(`feed_history answered ${history.reply}`);
        lastSeq = 0;
        items = [];
        accept(history.data.map((item) => ({ ...item, replayed: true })));
      }
      const sub = await request('subscribe_feed', { after: lastSeq, verbose: true });
      if (sub.reply !== 'subscribed') throw new Error(`subscribe_feed answered ${sub.reply}`);
      return sub.data;
    },
    push(item) {
      accept([item]);
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    get items() { return items; },
    get lastSeq() { return lastSeq; },
  };
}

// The quiet-chat filter: progress notifications only drive refreshes.
export function isQuiet(item) {
  return item.entry?.entry === 'notification' && item.entry.kind === 'progress';
}
