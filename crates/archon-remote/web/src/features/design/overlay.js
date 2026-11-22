// The stage: the frame's canvas on its desk, the overlay that draws selection
// outlines, pins and regions, and the pointer, touch and key handling that
// turns gestures into input events, hover reads, picks and regions.
import { h } from '../../ui/dom.js';
import { keyEvent } from './keymap.js';
import { mouseEvent, wheelEvent, touchEvent, modifiersOf } from './input.js';
import { toApp, effectiveScale, fitScale, boxToScreen, normalizeDrag, classifyTwoFinger, clampZoom, nudgeBox, centredBox } from './scale.js';
import { createSwitchGate } from './switching.js';

const DRAG_PX = 6;
const HOVER_MS = 100; // 10 reads a second

export function createStage({ getMode, onInput, onHover, onPick, onRegion, onCapture, onPin, onZoomChange = () => {} }) {
  const canvas = h('canvas', { class: 'design-canvas', width: '390', height: '844' });
  const ctx2d = canvas.getContext('2d');
  const hover = h('div', { class: 'design-sel is-hover', hidden: true }, h('span', { class: 'design-sel-label' }));
  const selected = h('div', { class: 'design-sel', hidden: true }, h('span', { class: 'design-sel-label' }));
  const regionBox = h('div', { class: 'design-region', hidden: true });
  const pins = h('div', { class: 'design-pins' });
  const layer = h('div', { class: 'design-layer', tabindex: '0', 'aria-label': 'Preview. Click to use the app; Escape stops typing into it.' }, hover, selected, regionBox, pins);
  const framebox = h('div', { class: 'design-framebox' }, canvas, layer);
  const desk = h('div', { class: 'design-desk' }, framebox);

  let frame = { w: 390, h: 844 };
  // The preset size being switched to: until its first frame, the box takes
  // that size and the old picture is hidden.
  let pending = null;
  let switchLabel = '';
  const switchGate = createSwitchGate({
    onChange(next) {
      pending = next;
      desk.classList.toggle('is-switching', Boolean(pending));
      framebox.dataset.switching = pending ? switchLabel : '';
      // The hidden layer's name is not read while hidden: the box says it.
      framebox.setAttribute('aria-busy', pending ? 'true' : 'false');
      if (pending) framebox.setAttribute('aria-label', switchLabel);
      else framebox.removeAttribute('aria-label');
      layout();
    },
  });
  let zoom = 1;
  let sizeMode = 'fit';
  let avail = { w: 0, h: 0 };
  let phone = false;
  let pinList = [];
  let selectedBox = null;
  let kbRegion = null;
  let capturing = false;
  let lastHover = 0;
  const touches = new Map();
  let twoPrev = null;
  let drag = null;

  const scale = () => effectiveScale(canvas.getBoundingClientRect(), frame);
  const point = (e) => toApp(e.clientX, e.clientY, canvas.getBoundingClientRect(), frame);

  function layout() {
    const base = fitScale({ frame: pending ?? frame, avail, mode: phone && sizeMode === 'fit' ? 'width' : sizeMode });
    const shown = pending ?? frame;
    const s = base * zoom;
    framebox.style.setProperty('--fw', `${Math.round(shown.w * s)}px`);
    framebox.style.setProperty('--fh', `${Math.round(shown.h * s)}px`);
    desk.classList.toggle('is-actual', sizeMode === 'actual');
    renderBoxes();
  }

  function place(el, box) {
    const s = boxToScreen(box, scale());
    for (const [k, v] of Object.entries({ '--x': s.x, '--y': s.y, '--w': s.w, '--h': s.h })) el.style.setProperty(k, `${v}px`);
  }

  function renderBoxes() {
    if (selectedBox) {
      place(selected, selectedBox);
      // The label chip flips below or to the right edge instead of leaving the canvas.
      const s = boxToScreen(selectedBox, scale());
      selected.classList.toggle('label-below', s.y < 30);
      selected.classList.toggle('label-end', s.x + 160 > canvas.getBoundingClientRect().width);
    }
    const edge = canvas.getBoundingClientRect();
    for (const pin of pins.children) {
      const box = pinList[Number(pin.dataset.index)]?.box;
      if (!box) continue;
      const s = boxToScreen({ x: box.x + box.w, y: box.y, w: 0, h: 0 }, scale());
      const half = 12; // the dot is 24 px: keep it inside the canvas
      const x = Math.min(Math.max(s.x, half), Math.max(half, edge.width - half));
      const y = Math.min(Math.max(s.y, half), Math.max(half, edge.height - half));
      pin.style.setProperty('--x', `${x}px`); pin.style.setProperty('--y', `${y}px`);
    }
    if (kbRegion) place(regionBox, kbRegion);
  }

  function bounds() {
    avail = { w: desk.clientWidth, h: desk.clientHeight };
    layout();
  }
  const resizeObserver = typeof ResizeObserver === 'function' ? new ResizeObserver(() => bounds()) : null;
  resizeObserver?.observe(desk);

  // ---- mouse and pen ----
  layer.addEventListener('contextmenu', (e) => e.preventDefault());
  layer.addEventListener('wheel', (e) => {
    if (getMode() !== 'interact') return;
    e.preventDefault();
    const p = point(e);
    onInput(wheelEvent(p, Math.round(e.deltaX), Math.round(e.deltaY), modifiersOf(e)));
  }, { passive: false });

  layer.addEventListener('pointerdown', (e) => {
    if (e.pointerType === 'touch') return touchDown(e);
    layer.setPointerCapture?.(e.pointerId);
    const mode = getMode();
    const p = point(e);
    if (mode === 'interact') {
      if (!capturing) { capturing = true; onCapture(true); }
      layer.focus({ preventScroll: true });
      onInput(mouseEvent('down', p, { button: e.button, clickCount: Math.max(1, e.detail), modifiers: modifiersOf(e) }));
    } else {
      drag = { start: p, box: null };
    }
  });
  layer.addEventListener('pointermove', (e) => {
    if (e.pointerType === 'touch') return touchMove(e);
    const mode = getMode();
    const p = point(e);
    if (mode === 'interact') {
      onInput(mouseEvent('move', p, { button: e.buttons ? (e.buttons & 1 ? 0 : e.buttons & 4 ? 1 : 2) : null, modifiers: modifiersOf(e) }));
    } else if (drag) {
      if (Math.hypot(p.x - drag.start.x, p.y - drag.start.y) * scale() > DRAG_PX || drag.box) {
        drag.box = normalizeDrag(drag.start, p, frame);
        regionBox.hidden = false;
        place(regionBox, drag.box);
      }
    } else if (mode === 'comment' && performance.now() - lastHover >= HOVER_MS) {
      lastHover = performance.now();
      onHover(p);
    }
  });
  layer.addEventListener('pointerup', (e) => {
    if (e.pointerType === 'touch') return touchUp(e);
    const mode = getMode();
    const p = point(e);
    if (mode === 'interact') {
      onInput(mouseEvent('up', p, { button: e.button, clickCount: Math.max(1, e.detail), modifiers: modifiersOf(e) }));
    } else if (drag) finishDrag(drag, p, mode);
    drag = null;
  });
  layer.addEventListener('pointerleave', () => { hover.hidden = true; });

  function finishDrag(d, p, mode) {
    regionBox.hidden = true;
    if (d.box && d.box.w >= 4 && d.box.h >= 4) onRegion(d.box);
    else if (mode === 'comment') onPick(p);
  }

  // ---- touch ----
  function touchDown(e) {
    layer.setPointerCapture?.(e.pointerId);
    touches.set(e.pointerId, { id: e.pointerId, ...point(e), cx: e.clientX, cy: e.clientY });
    const mode = getMode();
    if (mode !== 'interact') { drag = { start: point(e), box: null }; return; }
    if (touches.size === 2) {
      // The second finger takes over: cancel the app's touch, the gesture is local or a scroll.
      onInput(touchEvent('cancel', [...touches.values()].slice(0, 1)));
      twoPrev = [...touches.values()].map((t) => ({ x: t.cx, y: t.cy }));
    } else if (touches.size === 1) onInput(touchEvent('start', [...touches.values()]));
  }
  function touchMove(e) {
    const t = touches.get(e.pointerId);
    if (!t) return;
    Object.assign(t, point(e), { cx: e.clientX, cy: e.clientY });
    const mode = getMode();
    if (mode !== 'interact') {
      if (drag) {
        const p = point(e);
        if (Math.hypot(p.x - drag.start.x, p.y - drag.start.y) * scale() > DRAG_PX || drag.box) {
          drag.box = normalizeDrag(drag.start, p, frame);
          regionBox.hidden = false;
          place(regionBox, drag.box);
        }
      }
      return;
    }
    if (touches.size >= 2 && twoPrev) {
      const next = [...touches.values()].slice(0, 2).map((x) => ({ x: x.cx, y: x.cy }));
      const g = classifyTwoFinger(twoPrev, next);
      twoPrev = next;
      if (g.kind === 'zoom') {
        zoom = clampZoom(zoom * g.ratio);
        onZoomChange(zoom);
        layout();
      } else {
        const rect = canvas.getBoundingClientRect();
        const centre = toApp((next[0].x + next[1].x) / 2, (next[0].y + next[1].y) / 2, rect, frame);
        onInput(wheelEvent(centre, Math.round(-g.dx / scale()), Math.round(-g.dy / scale()), 0));
      }
    } else if (touches.size === 1) onInput(touchEvent('move', [...touches.values()]));
  }
  function touchUp(e) {
    const t = touches.get(e.pointerId);
    const mode = getMode();
    if (mode !== 'interact') {
      const p = point(e);
      if (drag) finishDrag(drag, p, mode);
      drag = null;
      touches.delete(e.pointerId);
      return;
    }
    if (t && touches.size === 1) onInput(touchEvent(e.type === 'pointercancel' ? 'cancel' : 'end', [t]));
    touches.delete(e.pointerId);
    if (touches.size < 2) twoPrev = null;
  }
  layer.addEventListener('pointercancel', touchUp);

  // ---- keys: forwarded while capturing; arrows and Enter move a region ----
  layer.addEventListener('keydown', (e) => {
    const mode = getMode();
    if (mode === 'interact' && capturing) {
      if (e.key === 'Escape') { capturing = false; onCapture(false); e.preventDefault(); e.stopPropagation(); return; }
      const ev = keyEvent('down', e);
      if (ev) { e.preventDefault(); onInput(ev); }
    } else if (mode === 'region') {
      if (e.key.startsWith('Arrow')) {
        e.preventDefault();
        kbRegion = nudgeBox(kbRegion ?? centredBox(frame), e.key, e.shiftKey, frame);
        regionBox.hidden = false;
        place(regionBox, kbRegion);
      } else if (e.key === 'Enter' && kbRegion) {
        e.preventDefault();
        const box = kbRegion;
        kbRegion = null;
        regionBox.hidden = true;
        onRegion(box);
      } else if (e.key === 'Escape') {
        kbRegion = null;
        regionBox.hidden = true;
      }
    }
  });
  layer.addEventListener('keyup', (e) => {
    if (getMode() === 'interact' && capturing) {
      const ev = keyEvent('up', e);
      if (ev) { e.preventDefault(); onInput(ev); }
    }
  });
  layer.addEventListener('blur', () => { if (capturing) { capturing = false; onCapture(false); } });

  return {
    element: desk, canvas, layer,
    draw(bitmap, meta) {
      if (meta.w !== frame.w || meta.h !== frame.h) {
        frame = { w: meta.w, h: meta.h };
        canvas.width = meta.w;
        canvas.height = meta.h;
        layout();
      }
      ctx2d.drawImage(bitmap, 0, 0, frame.w, frame.h);
      bitmap.close?.();
    },
    get frame() { return frame; },
    get capturing() { return capturing; },
    releaseCapture() { if (capturing) { capturing = false; onCapture(false); } },
    resize: bounds,
    setPhone(value) { phone = value; layout(); },
    // `size` is the preset being switched to (null once its frames arrive);
    // `label` says so in the empty box.
    // Time-limited (switching.js): a stalled switch shows the stream again.
    setSwitching(size, label = '') {
      switchLabel = label;
      switchGate.set(size);
    },
    toggleActual() { sizeMode = sizeMode === 'actual' ? 'fit' : 'actual'; zoom = 1; layout(); return sizeMode; },
    get sizeMode() { return sizeMode; },
    setHover(box, label) {
      hover.hidden = !box;
      if (box) { place(hover, box); hover.firstChild.textContent = label ?? ''; }
    },
    setSelection(box, label) {
      selectedBox = box;
      selected.hidden = !box;
      if (box) { selected.firstChild.textContent = label ?? ''; place(selected, box); }
    },
    setPins(list) {
      pinList = list;
      pins.replaceChildren(...list.map((p, i) => h('button', { class: ['design-pin', p.tone && `tone-${p.tone}`], type: 'button', dataset: { index: String(i) },
        'aria-label': `Note ${p.id}: ${p.label}`, onclick: () => onPin(p.id) }, h('span', { class: 'design-pin-dot', text: String(p.number) }))));
      renderBoxes();
    },
    // The box a region drawn from the keyboard starts with.
    startKeyboardRegion() { kbRegion = centredBox(frame); regionBox.hidden = false; place(regionBox, kbRegion); layer.focus({ preventScroll: true }); },
    clearRegion() { kbRegion = null; regionBox.hidden = true; },
    destroy() { resizeObserver?.disconnect(); switchGate.dispose(); },
  };
}
