// While the preview switches size the old picture is hidden, until the first
// frame at the new size arrives. A switch that never completes (the browser
// stalled, the agent's app is slow to lay out) must not leave the stage
// blank: after `limitMs` the hide gives up and the stream shows whatever it
// sends, until a different size is asked for.

export const SWITCH_LIMIT_MS = 8000;

export function createSwitchGate({
  onChange,
  limitMs = SWITCH_LIMIT_MS,
  setTimer = globalThis.setTimeout.bind(globalThis),
  clearTimer = globalThis.clearTimeout.bind(globalThis),
}) {
  let pending = null;
  let gaveUp = null;
  let timer = null;
  const same = (a, b) => a?.w === b?.w && a?.h === b?.h;
  const stop = () => { if (timer !== null) { clearTimer(timer); timer = null; } };
  return {
    // The size being switched to, or null when nothing is hidden.
    get pending() { return pending; },
    // `size` is the preset being switched to, or null once its frames arrive.
    set(size) {
      const next = size ? { w: size.w, h: size.h } : null;
      if (!next) gaveUp = null;
      if (next && same(next, gaveUp)) return;
      if (same(next, pending)) return;
      stop();
      pending = next;
      if (pending) {
        timer = setTimer(() => {
          timer = null;
          gaveUp = pending;
          pending = null;
          onChange(null);
        }, limitMs);
      }
      onChange(pending);
    },
    dispose() { stop(); },
  };
}
