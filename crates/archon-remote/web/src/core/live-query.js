// A visible screen owns its reads, coalesces feed and change bursts and
// releases them on exit. Polling only covers what no event announces.
export const MIN_POLL_MS = 5000;
const COALESCE_MS = 350;

export function liveQuery(ctx, read, receive, { interval = 15000 } = {}) {
  if (typeof read !== 'function' || typeof receive !== 'function') throw new TypeError('liveQuery needs a read and receiver');
  if (!Number.isFinite(interval) || interval < MIN_POLL_MS) throw new RangeError(`poll interval must be at least ${MIN_POLL_MS} ms`);
  if (typeof ctx?.feed?.subscribe !== 'function' || typeof ctx?.changes?.subscribe !== 'function') throw new TypeError('liveQuery needs the feed and the change stream');
  let alive = true;
  let running = false;
  let pending = false;
  let timer = null;
  const refresh = async () => {
    if (!alive) return;
    if (running) { pending = true; return; }
    running = true;
    try {
      const data = await read();
      if (alive) receive({ data, error: null });
    } catch (error) {
      if (alive) receive({ data: null, error });
    } finally {
      running = false;
      if (pending && alive) { pending = false; refresh(); }
    }
  };
  // The first event of a burst schedules one read; a steady stream cannot postpone it.
  const soon = () => {
    if (timer !== null) return;
    timer = setTimeout(() => { timer = null; refresh(); }, COALESCE_MS);
  };
  const offFeed = ctx.feed.subscribe(soon);
  const offChanges = ctx.changes.subscribe(soon);
  const offConnection = ctx.store.watch((s) => s.connection.state, (state) => state === 'open' && soon());
  const visible = () => document.visibilityState === 'visible' && ctx.store.get().connection.state === 'open';
  const onVisible = () => visible() && refresh();
  document.addEventListener('visibilitychange', onVisible);
  const poll = setInterval(onVisible, interval);
  refresh();
  return {
    refresh,
    destroy() {
      alive = false;
      clearTimeout(timer);
      clearInterval(poll);
      offFeed();
      offChanges();
      offConnection();
      document.removeEventListener('visibilitychange', onVisible);
    },
  };
}
