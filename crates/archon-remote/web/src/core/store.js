// One observable state object. Listeners get the whole state and pick out
// what they render; `watch` calls back only when the picked value changes
// (an array pick compares element by element).
export function createStore(initial) {
  let state = Object.freeze({ ...initial });
  const listeners = new Set();
  return {
    get: () => state,
    set(patch) {
      if (!patch || typeof patch !== 'object') throw new TypeError('a store patch is an object');
      state = Object.freeze({ ...state, ...patch });
      for (const listener of [...listeners]) listener(state);
    },
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    watch(pick, listener) {
      let last = pick(state);
      listener(last, state);
      const off = this.subscribe((next) => {
        const value = pick(next);
        if (same(value, last)) return;
        last = value;
        listener(value, next);
      });
      return off;
    },
  };
}

function same(a, b) {
  if (a === b) return true;
  return Array.isArray(a) && Array.isArray(b) && a.length === b.length && a.every((x, i) => x === b[i]);
}
