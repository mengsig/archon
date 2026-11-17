// A visible screen owns its reads, coalesces feed bursts and releases them on exit.
export function liveQuery(ctx, read, receive, { interval = 15000 } = {}) {
  if (typeof read !== 'function' || typeof receive !== 'function') throw new TypeError('liveQuery needs a read and receiver');
  if (!Number.isFinite(interval) || interval < 1000) throw new RangeError('poll interval must be at least one second');
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
  const soon = () => {
    clearTimeout(timer);
    timer = setTimeout(refresh, 350);
  };
  const offFeed = ctx.feed.subscribe(soon);
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
      offConnection();
      document.removeEventListener('visibilitychange', onVisible);
    },
  };
}
