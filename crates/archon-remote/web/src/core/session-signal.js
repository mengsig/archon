// Tells this browser's other tabs that its pairing changed, and hears them. Tabs
// share one session cookie, so a pairing or a forget in one changes them all.
const CHANNEL = 'archon-session';
const STORAGE_KEY = 'archon-session-changed';
const REASONS = new Set(['paired', 'forgotten']);

// BroadcastChannel where the browser has it, else a storage write, which fires a
// `storage` event in every other tab. Neither reaches the tab that sent it.
export function createSessionSignal({ Channel = globalThis.BroadcastChannel, storage = globalThis.localStorage, events = globalThis } = {}) {
  const channel = typeof Channel === 'function' ? new Channel(CHANNEL) : null;
  if (!channel && typeof storage?.setItem !== 'function') throw new TypeError('the session signal needs a channel or storage');
  if (!channel && typeof events?.addEventListener !== 'function') throw new TypeError('the session signal listens for storage events');
  return {
    announce(reason) {
      if (!REASONS.has(reason)) throw new RangeError(`not a session change: ${reason}`);
      if (channel) {
        channel.postMessage({ reason });
        return;
      }
      try {
        storage.setItem(STORAGE_KEY, `${reason} ${Date.now()} ${Math.random()}`);
      } catch (err) {
        // Storage refused (a private window, say): other tabs see it when next shown.
        console.warn('Other open tabs notice this change when they are next shown.', err);
      }
    },
    // Returns the unsubscribe.
    listen(onChange) {
      if (typeof onChange !== 'function') throw new TypeError('a session listener is a function');
      if (channel) {
        const heard = (event) => REASONS.has(event.data?.reason) && onChange(event.data.reason);
        channel.addEventListener('message', heard);
        return () => channel.removeEventListener('message', heard);
      }
      const heard = (event) => event.key === STORAGE_KEY && event.newValue && onChange(event.newValue.split(' ')[0]);
      events.addEventListener('storage', heard);
      return () => events.removeEventListener('storage', heard);
    },
  };
}

let shared = null;

// The page's one signal: its own announcements never echo back to it.
export function sessionSignal() {
  shared ??= createSessionSignal();
  return shared;
}
