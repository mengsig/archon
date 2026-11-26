// The preview falling behind, as one quiet status line that replaces and never
// stacks: a notice (input skipped or lost) for a while, else whether input waits.
export const BEHIND = {
  busy: 'Catching up with the preview…',
  skipped: 'The preview fell behind; some input was skipped.',
  lost: 'The preview stopped answering; some input may not have reached it.',
};
export const NOTICE_MS = 4000;

export function createBehindLine({ show, setTimer = (fn, ms) => setTimeout(fn, ms), clearTimer = (t) => clearTimeout(t) }) {
  if (typeof show !== 'function') throw new TypeError('the behind line needs a show function');
  let waiting = false;
  let notice = null;
  let timer = null;
  let shown;
  const render = () => {
    const line = notice ?? (waiting ? BEHIND.busy : null);
    if (line !== shown) show(line);
    shown = line;
  };
  return {
    // Input waits on the preview; it stays said until it no longer does.
    waiting(on) {
      waiting = Boolean(on);
      render();
    },
    // Said for NOTICE_MS whatever `waiting` does meanwhile; a newer one replaces it.
    notice(message) {
      if (typeof message !== 'string' || message === '') throw new TypeError('a notice is a message');
      if (timer !== null) clearTimer(timer);
      notice = message;
      timer = setTimer(() => { timer = null; notice = null; render(); }, NOTICE_MS);
      render();
    },
    stop() {
      if (timer !== null) clearTimer(timer);
      timer = null;
    },
  };
}
