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
  ctx, item, onView, onEvent, onFrame, onError, visible = () => document.visibilityState === 'visible',
  decode = (b64) => createImageBitmap(new Blob([decodeBase64(b64)], { type: 'image/jpeg' })),
  setTimer = (fn, ms) => setTimeout(fn, ms),
}) {
  let sub = null;
  let sessionId = null;
  let alive = true;
  let generation = 0;
  let decoding = false;
  let latest = null;
  let lastAck = 0;
  let started = false;

  // start: a fresh subscription (first load, reconnect). refresh: design_get only; the
  // subscription is kept unless the session id changed or there is none yet. The daemon
  // caps subscriptions per connection and has no unsubscribe, so never resubscribe needlessly.
  async function load(fresh) {
    started = true;
    const mine = ++generation;
    if (fresh) sub = null;
    try {
      const { kind, data } = await ctx.design.get(item);
      if (!alive || mine !== generation) return;
      onView(kind, data);
      const session = kind === 'design_state' ? data.session : null;
      const changed = (session?.id ?? null) !== sessionId;
      sessionId = session?.id ?? null;
      if (changed) sub = null;
      if (!session || session.state === 'ended' || session.state === 'confirmed' || !ctx.canOperate) return;
      if (sub !== null) return;
      const next = await ctx.design.subscribe(session.id);
      if (!alive || mine !== generation) return;
      sub = next;
    } catch (err) {
      if (alive && mine === generation) onError(err);
    }
  }

  const start = () => load(true);
  const refresh = () => load(false);

  const offHub = ctx.designHub.subscribe((frameSub, event) => {
    if (frameSub !== sub) return;
    if (event.event === 'frame') {
      latest = event;
      drain();
      return;
    }
    onEvent(event);
  });

  async function drain() {
    if (decoding) return;
    decoding = true;
    try {
      while (latest && alive) {
        const frame = latest;
        latest = null;
        let bitmap = null;
        try {
          bitmap = await decode(frame.jpeg_b64);
        } catch {
          continue; // a frame that does not decode is skipped; the next one replaces it
        }
        if (!alive) { bitmap?.close?.(); continue; }
        onFrame(bitmap, frame);
        onEvent({ event: 'frame', seq: frame.seq, w: frame.w, h: frame.h, at: frame.at });
        ack(frame.seq);
      }
    } finally {
      decoding = false;
    }
  }

  // Visible tabs ack at once; hidden ones at most twice a second, which drops the rate to 2 fps.
  function ack(seq) {
    const send = () => {
      if (!alive || sub === null) return;
      lastAck = Date.now();
      ctx.design.ack(sub, seq).catch(() => {});
    };
    if (visible() || Date.now() - lastAck >= HIDDEN_ACK_DELAY_MS) send();
    else setTimer(send, HIDDEN_ACK_DELAY_MS - (Date.now() - lastAck));
  }

  // A reconnect starts over: the old sub died with the socket.
  let wasOpen = ctx.store.get().connection.state === 'open';
  const offConn = ctx.store.watch((s) => s.connection.state, (state) => {
    const open = state === 'open';
    onEvent({ type: 'connection', state });
    if (open && !wasOpen && started) start();
    wasOpen = open;
  });

  return {
    start,
    refresh,
    get session() { return sessionId; },
    get sub() { return sub; },
    stop() {
      alive = false;
      offHub();
      offConn();
    },
  };
}
