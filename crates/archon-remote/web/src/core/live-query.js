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
  const onPoll = () => visible() && refresh();
  // A hidden tab's throttled timer may still hold a burst's read: run it now, once.
  const onVisible = () => {
    if (!visible()) return;
    clearTimeout(timer);
    timer = null;
    refresh();
  };
  document.addEventListener('visibilitychange', onVisible);
  const poll = setInterval(onPoll, interval);
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

// Calls `fn` once, `ms` after the first of a burst of calls; `flush` runs a
// pending call at once, e.g. when a throttled background tab is shown again.
export function coalesce(fn, ms) {
  if (typeof fn !== 'function') throw new TypeError('coalesce needs a function');
  if (!Number.isFinite(ms) || ms < 0) throw new RangeError(`coalesce needs a nonnegative delay, got ${ms}`);
  let timer = null;
  const run = () => {
    clearTimeout(timer);
    timer = null;
    fn();
  };
  const schedule = () => {
    if (timer === null) timer = setTimeout(run, ms);
  };
  schedule.flush = run;
  schedule.pending = () => timer !== null;
  return schedule;
}

// No change announces a pause or resume, so a screen that shows the pause state
// reads status once per `interval` while the tab is shown and connected.
export function pollStatus(ctx, { interval = MIN_POLL_MS } = {}) {
  if (typeof ctx?.refresh?.one !== 'function' || typeof ctx?.store?.get !== 'function') throw new TypeError('pollStatus needs the status loader and the store');
  if (!Number.isFinite(interval) || interval < MIN_POLL_MS) throw new RangeError(`poll interval must be at least ${MIN_POLL_MS} ms`);
  const poll = setInterval(() => {
    if (document.visibilityState === 'visible' && ctx.store.get().connection.state === 'open') ctx.refresh.one('status');
  }, interval);
  return () => clearInterval(poll);
}
