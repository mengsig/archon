// Where this app is served from: "" at a listener's root, "/archon" behind
// Aether's mount. The app's pages ("/" and "/pair") sit in its own directory.
export function appBase(documentUrl = document.URL) {
  const path = new URL(documentUrl).pathname;
  const slash = path.lastIndexOf('/');
  if (slash < 0) throw new Error(`the app path must be absolute: ${path}`);
  return path.slice(0, slash);
}

export const BASE = typeof document === 'undefined' ? '' : appBase();
