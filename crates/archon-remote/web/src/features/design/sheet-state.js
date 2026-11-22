// The notes sheet around the composer on phone and tablet. Opening the
// composer raises the sheet and focuses its textarea, which can scroll the
// page; closing it without sending must undo both, or the stage stays covered.

// Sheet height once the composer closes without a send. Escape with no
// composer open (clearing a region, say) leaves the sheet where it is.
export function sheetAfterCancel(layout, sheetHeight, composerWasOpen = true) {
  return layout === 'desktop' || !composerWasOpen ? sheetHeight : 'peek';
}

// The page's scroll position, to put back when the composer closes.
export function readScroll(win = globalThis.window) {
  const el = win?.document?.scrollingElement;
  return { x: el?.scrollLeft ?? win?.scrollX ?? 0, y: el?.scrollTop ?? win?.scrollY ?? 0 };
}

export function restoreScroll(pos, win = globalThis.window) {
  if (!pos || !win) return;
  const el = win.document?.scrollingElement;
  if (el && typeof el.scrollTo === 'function') el.scrollTo(pos.x, pos.y);
  else win.scrollTo?.(pos.x, pos.y);
}
