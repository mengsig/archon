// Where this app is served from: "" at a listener's root, "/archon" behind
// Aether's mount. index.html's <base href="./"> makes the document base the
// app's own directory, whatever page path the browser opened.
export function appBase(baseUri = document.baseURI) {
  const path = new URL(baseUri).pathname;
  if (!path.endsWith('/')) throw new Error(`the app base must end in "/": ${path}`);
  return path.slice(0, -1);
}

export const BASE = typeof document === 'undefined' ? '' : appBase();
