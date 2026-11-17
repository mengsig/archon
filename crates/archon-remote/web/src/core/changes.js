// The change stream: archond's committed state changes (items, runs, stages,
// questions, links), live only. Each reconnect reloads every view anyway.
export const CHANGES_FEATURE = 'changes';

export function createChanges({ request }) {
  if (typeof request !== 'function') throw new TypeError('the change stream needs a request function');
  const listeners = new Set();
  return {
    // Called after every welcome; a daemon without the feature streams nothing.
    async sync(welcome) {
      if (!Array.isArray(welcome?.features)) throw new TypeError('a welcome lists its features');
      if (!welcome.features.includes(CHANGES_FEATURE)) return null;
      const sub = await request('subscribe_changes', { after: null });
      if (sub?.reply !== 'subscribed') throw new Error(`subscribe_changes answered ${sub?.reply}`);
      return sub.data;
    },
    push(change) {
      if (!Number.isInteger(change?.seq) || typeof change.subject !== 'string') throw new TypeError('a change has a seq and a subject');
      for (const listener of [...listeners]) listener(change);
    },
    subscribe(listener) {
      if (typeof listener !== 'function') throw new TypeError('a change listener is a function');
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}
