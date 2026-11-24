// The live connection to one design session: design_get, subscribe_design,
// frames drawn latest-first and acknowledged, and a fresh start after a reconnect.
export const HIDDEN_ACK_DELAY_MS = 500;

export function decodeBase64(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

export function createDesignStream({
  ctx, item, onView, onEvent, onFrame, onError, onReset = () => {}, visible = () => document.visibilityState === 'visible',
  decode = (b64) => createImageBitmap(new Blob([decodeBase64(b64)], { type: 'image/jpeg' })),
  setTimer = (fn, ms) => setTimeout(fn, ms), clearTimer = (timer) => clearTimeout(timer), now = () => performance.now(),
}) {
  let active = null;
  let sessionId = null;
  let alive = true;
  let generation = 0;
  let started = false;
  let pending = null; // the one subscribe in flight: {session, promise}

  function detach() {
    if (active?.ackTimer != null) clearTimer(active.ackTimer);
    if (active) active.latest = null;
    active = null;
    pending = null;
  }

  // start: a fresh subscription (first load, reconnect). refresh: design_get only; the
  // subscription is kept unless the session id changed or there is none yet. The daemon
  // caps subscriptions per connection and has no unsubscribe request, so never resubscribe needlessly.
  async function load(fresh) {
    if (!alive) return;
    started = true;
    if (!wasOpen) return;
    const mine = ++generation;
    if (fresh) { detach(); onReset(); }
    try {
      const { kind, data } = await ctx.design.get(item);
      if (!alive || mine !== generation) return;
      const session = kind === 'design_state' ? data.session : null;
      const changed = (session?.id ?? null) !== sessionId;
      sessionId = session?.id ?? null;
      if (changed) { detach(); if (!fresh) onReset(); }
      onView(kind, data);
      if (!alive || mine !== generation) return;
      if (!session || session.state === 'ended' || session.state === 'confirmed' || !ctx.canOperate) return;
      if (active) return;
      // A refresh while a subscribe is in flight joins it: one live subscription at a time.
      if (!pending || pending.session !== session.id) {
        const p = { session: session.id };
        pending = p;
        p.promise = ctx.design.subscribe(session.id).then((id) => {
          if (pending === p && alive && sessionId === p.session) {
            console.assert(Number.isInteger(id) && active === null, 'Invalid design subscription transition');
            active = { id, latest: null, decoding: false, lastAck: -Infinity, ackSeq: 0, ackTimer: null };
          }
        }).finally(() => { if (pending === p) pending = null; });
      }
      await pending.promise;
    } catch (err) {
      if (alive && mine === generation) onError(err);
    }
  }

  const start = () => load(true);
  const refresh = () => load(false);

  const offHub = ctx.designHub.subscribe((frameSub, event) => {
    const source = active;
    if (!source || frameSub !== source.id) return;
    if (event.event === 'frame') {
      source.latest = event;
      drain(source).catch((err) => { if (active === source) onError(err); });
      return;
    }
    if (event.event === 'ended') detach();
    onEvent(event);
  });

  async function drain(source) {
    if (source.decoding) return;
    source.decoding = true;
    try {
      while (active === source && source.latest) {
        const frame = source.latest;
        source.latest = null;
        let bitmap;
        try {
          bitmap = await decode(frame.jpeg_b64);
        } catch {
          // A bad JPEG still consumed the relay's frame slot.
          ack(source, frame.seq);
          continue;
        }
        try {
          if (active !== source) return;
          onFrame(bitmap, frame);
          onEvent({ event: 'frame', seq: frame.seq, w: frame.w, h: frame.h, at: frame.at });
        } finally {
          bitmap.close?.();
          ack(source, frame.seq);
        }
      }
    } finally {
      source.decoding = false;
    }
  }

  // ACKs are cumulative. Keep only the newest sequence while a hidden tab waits.
  function ack(source, seq) {
    if (active !== source) return;
    console.assert(Number.isInteger(seq) && seq > 0, 'Invalid design frame sequence');
    source.ackSeq = Math.max(source.ackSeq, seq);
    const send = () => {
      source.ackTimer = null;
      if (active !== source) return;
      source.lastAck = now();
      ctx.design.ack(source.id, source.ackSeq).catch((err) => {
        if (active === source && err?.code !== 'disconnected') onError(err);
      });
    };
    const wait = source.lastAck + HIDDEN_ACK_DELAY_MS - now();
    if (visible() || wait <= 0) {
      if (source.ackTimer !== null) clearTimer(source.ackTimer);
      send();
    } else if (source.ackTimer === null) {
      source.ackTimer = setTimer(send, wait);
    }
  }

  let wasOpen = ctx.store.get().connection.state === 'open';
  const offConn = ctx.store.watch((s) => s.connection.state, (state) => {
    const open = state === 'open';
    const reconnect = open && !wasOpen;
    wasOpen = open;
    if (!open) { generation += 1; detach(); }
    if (reconnect && started) start();
    onEvent({ type: 'connection', state });
  });

  return {
    start,
    refresh,
    get session() { return sessionId; },
    get sub() { return active?.id ?? null; },
    stop() {
      alive = false;
      detach();
      offHub();
      offConn();
    },
  };
}
