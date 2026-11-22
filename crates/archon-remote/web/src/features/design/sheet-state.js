// The notes sheet around the composer on phone and tablet. Opening the
// composer raises the sheet and focuses its textarea; tapping its buttons or
// focusing a field can scroll the page, the stage column and the sheet itself.
// Closing it without sending must undo all of that, or the stage stays covered.

// Sheet height once the composer closes without a send. Escape with no
// composer open (clearing a region, say) leaves the sheet where it is.
export function sheetAfterCancel(layout, sheetHeight, composerWasOpen = true) {
  return layout === 'desktop' || !composerWasOpen ? sheetHeight : 'peek';
}

// The scroll position of the page and of each given scroll container, to put
// back when the composer closes. Elements that are missing are skipped.
export function readScroll(elements = [], win = globalThis.window) {
  const page = win?.document?.scrollingElement;
  const all = [page, ...elements].filter(Boolean);
  return all.map((el) => ({ el, x: el.scrollLeft ?? 0, y: el.scrollTop ?? 0 }));
}

export function restoreScroll(saved) {
  for (const { el, x, y } of saved ?? []) {
    if (typeof el.scrollTo === 'function') el.scrollTo(x, y);
    else { el.scrollLeft = x; el.scrollTop = y; }
  }
}
