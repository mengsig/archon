// The preview's size preset, changed one request at a time per session.
// `confirmed` is what archond last acknowledged (or the frames show, after a switch
// was given up), `pending` the request in flight and `queued` the newest choice waiting
// for it; `settles` counts answered requests, so a view asked for before one was
// answered is known to be stale.

const PRESETS = new Set(['desktop', 'tablet', 'phone']);

// The operator only picks known presets; archond's own may be newer than this app.
function assertPreset(preset) {
  if (!PRESETS.has(preset)) throw new RangeError(`no viewport preset ${preset}`);
}
function assertNamed(preset) {
  if (typeof preset !== 'string' || !preset) throw new TypeError(`a viewport preset is a name, not ${preset}`);
}

export function initialViewport(preset = 'desktop', settles = 0) {
  assertNamed(preset);
  if (!Number.isInteger(settles) || settles < 0) throw new RangeError(`bad settle count ${settles}`);
  return { confirmed: preset, pending: null, queued: null, lastSent: null, retried: null, abandoned: null, settles };
}

// The preset the screen shows as chosen: the newest choice, else what archond has.
export function chosenPreset(vp) {
  return vp.queued ?? vp.pending?.preset ?? vp.confirmed;
}

// A newer choice replaces any queued one; one equal to what will stand needs no request.
// The operator's choice also earns a stalled switch its one more try.
export function choose(vp, preset) {
  assertPreset(preset);
  const standing = vp.pending ? vp.pending.preset : vp.confirmed;
  return { ...vp, queued: preset === standing ? null : preset, retried: null, abandoned: null };
}

// Frames at another size than archond confirmed, with nothing in flight: ask for it again, once per preset.
export function resend(vp, session) {
  if (vp.pending || vp.queued || vp.lastSent === `${session}|${vp.confirmed}`) return vp;
  return { ...vp, queued: vp.confirmed };
}

// archond confirmed a preset its frames have not reached for a while. Ask for it
// once more, then keep the preset the frames show, so a switch always ends.
export function stalled(vp, session, shown) {
  if (vp.pending || vp.queued) return { vp, gaveUp: false };
  assertNamed(session);
  const asked = `${session}|${vp.confirmed}`;
  if (vp.retried !== asked) return { vp: { ...vp, queued: vp.confirmed, retried: asked }, gaveUp: false };
  if (!shown || shown === vp.confirmed) return { vp, gaveUp: false };
  assertPreset(shown);
  // archond is asked for the frames' size once; refused or not, it stands until the next choice.
  const first = vp.abandoned !== asked;
  return { vp: { ...vp, confirmed: shown, queued: first ? shown : null, abandoned: asked }, gaveUp: first };
}

// The request to send now, or null while one is in flight.
export function nextRequest(vp) {
  return vp.pending ? null : vp.queued;
}

export function sending(vp, session, preset) {
  if (vp.pending || vp.queued !== preset) throw new Error(`a viewport request for ${preset} is not the one due`);
  if (!session) throw new Error('a viewport request names its session');
  return { ...vp, pending: { session, preset }, queued: null, lastSent: `${session}|${preset}` };
}

// The in-flight request was answered. A refusal or error keeps the confirmed preset.
export function settled(vp, { session, preset, ok }) {
  if (vp.pending?.session !== session || vp.pending.preset !== preset) return vp;
  const confirmed = ok ? preset : vp.confirmed;
  const queued = vp.queued === confirmed ? null : vp.queued;
  return { ...vp, confirmed, pending: null, queued, settles: vp.settles + 1 };
}

// A view's preset counts only if it was asked for after the last answer and nothing is in flight.
export function fromView(vp, preset, stamp) {
  if (!preset || vp.pending || stamp !== vp.settles) return vp;
  assertNamed(preset);
  return { ...vp, confirmed: preset, queued: vp.queued === preset ? null : vp.queued };
}
