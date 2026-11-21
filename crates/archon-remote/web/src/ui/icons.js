// Line icons drawn as SVG paths on a 24-unit grid, stroked in currentColor.
const SVG = 'http://www.w3.org/2000/svg';
const PATHS = {
  graph: ['M4 4h5v5H4z', 'M15 15h5v5h-5z', 'M4 15h5v5H4z', 'M6.5 9v6', 'M9 6.5h8.5V15'],
  mark: ['M12 2l10 10-10 10L2 12z', 'M12 8l4 4-4 4-4-4z'],
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
  clock: ['M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16z', 'M12 8v4l3 2'],
  working: ['M12 3a9 9 0 1 0 9 9', 'M12 3v9h9'],
  help: ['M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18z', 'M9.5 9.5a2.5 2.5 0 1 1 3.5 2.3c-.7.4-1 .9-1 1.7', 'M12 17v.5'],
  alert: ['M12 3l10 18H2z', 'M12 10v5', 'M12 18v.5'],
  undo: ['M9 7L4 12l5 5', 'M4 12h10a6 6 0 0 1 0 12'],
  compare: ['M4 5h7v14H4z', 'M13 5h7v14h-7z'],
  pencil: ['M4 20l1-5L16 4l4 4L9 19z', 'M14 6l4 4'],
  pointer: ['M6 3l12 8-5 1.5L11 18z'],
  region: ['M4 8V4h4', 'M16 4h4v4', 'M20 16v4h-4', 'M8 20H4v-4'],
  notes: ['M5 4h14v16H5z', 'M8 9h8', 'M8 13h8', 'M8 17h5'],
  sketch: ['M4 20c4-1 5-6 8-6s2 4 8 2', 'M14 4l6 6-9 9'],
  image: ['M4 5h16v14H4z', 'M4 16l5-5 4 4 3-3 4 4', 'M9 9.5a1 1 0 1 0 0-.1'],
  keyboard: ['M3 7h18v10H3z', 'M7 11h.5', 'M11 11h.5', 'M15 11h.5', 'M8 14h8'],
  reload: ['M20 12a8 8 0 1 1-2.3-5.7', 'M20 4v4h-4'],
  flag: ['M6 3v18', 'M6 4h12l-3 4 3 4H6'],
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
