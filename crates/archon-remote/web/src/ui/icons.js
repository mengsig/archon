// Line icons drawn as SVG paths on a 24-unit grid, stroked in currentColor.
const SVG = 'http://www.w3.org/2000/svg';
const PATHS = {
  queue: ['M4 6h16', 'M4 12h16', 'M4 18h10'],
  inbox: ['M3 13l3-8h12l3 8', 'M3 13v6h18v-6', 'M3 13h5l1 3h6l1-3h5'],
  chat: ['M4 5h16v11H9l-5 4z'],
  overview: ['M4 4h7v7H4z', 'M13 4h7v4h-7z', 'M13 10h7v10h-7z', 'M4 13h7v7H4z'],
  plus: ['M12 5v14', 'M5 12h14'],
  back: ['M15 5l-7 7 7 7'],
  chevron: ['M9 6l6 6-6 6'],
  settings: ['M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z', 'M19 12l2-1-1-3-2.2.2-1.4-1.6.4-2.2-3-1.4-1.3 1.8h-1l-1.3-1.8-3 1.4.4 2.2L6.2 8.2 4 8 3 11l2 1v1l-2 1 1 3 2.2-.2 1.4 1.6-.4 2.2 3 1.4 1.3-1.8h1l1.3 1.8 3-1.4-.4-2.2 1.4-1.6L20 16l1-3-2-1z'],
  pause: ['M8 5v14', 'M16 5v14'],
  play: ['M7 5l12 7-12 7z'],
  search: ['M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14z', 'M20 20l-4-4'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
  check: ['M5 12l5 5 9-10'],
  send: ['M12 19V5', 'M6 11l6-6 6 6'],
  external: ['M14 4h6v6', 'M20 4l-9 9', 'M18 14v6H4V6h6'],
  file: ['M6 3h8l4 4v14H6z', 'M14 3v4h4'],
  retry: ['M4 12a8 8 0 1 0 2.3-5.7', 'M4 4v4h4'],
  stop: ['M6 6h12v12H6z'],
  branch: ['M6 3v12', 'M18 9a3 3 0 1 0 0-.1', 'M6 21a3 3 0 1 0 0-.1', 'M18 9c0 6-12 3-12 9'],
};

export function icon(name, { size = 22, label = null } = {}) {
  const paths = PATHS[name];
  if (!paths) throw new RangeError(`no icon named ${name}`);
  const svg = document.createElementNS(SVG, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('class', 'icon');
  if (label) {
    svg.setAttribute('role', 'img');
    svg.setAttribute('aria-label', label);
  } else {
    svg.setAttribute('aria-hidden', 'true');
  }
  for (const d of paths) {
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}
