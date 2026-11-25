// Sketch: draw on a frozen frame or a blank "New screen", or upload an image.
// The result goes to design_upload and comes back as the composer's picture.
import { h } from '../../ui/dom.js';
import { icon } from '../../ui/icons.js';
import { openSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { createSketch, fitWithin, UPLOAD_TYPES } from './sketch-model.js';
import { MAX_ENCODED_IMAGE_BYTES, MAX_IMAGE_SIDE, designErrorText } from '../../core/design.js';

const INK = '#d1242f';

export function drawItems(g, items) {
  g.lineWidth = 3; g.lineCap = 'round'; g.lineJoin = 'round'; g.strokeStyle = INK; g.fillStyle = INK;
  for (const it of items) {
    g.beginPath();
    if (it.tool === 'pen') { it.points.forEach(([x, y], i) => (i ? g.lineTo(x, y) : g.moveTo(x, y))); g.stroke(); }
    else if (it.tool === 'rect') g.strokeRect(it.x, it.y, it.w, it.h);
    else if (it.tool === 'arrow') {
      const a = Math.atan2(it.y2 - it.y1, it.x2 - it.x1);
      g.moveTo(it.x1, it.y1); g.lineTo(it.x2, it.y2);
      for (const d of [0.5, -0.5]) { g.moveTo(it.x2, it.y2); g.lineTo(it.x2 - 16 * Math.cos(a - d), it.y2 - 16 * Math.sin(a - d)); }
      g.stroke();
    } else { g.font = '600 20px sans-serif'; g.fillText(it.text, it.x, it.y); }
  }
}

// Re-encode until the picture fits the cap: a smaller size or a lower quality.
export async function encodeUnder(canvas, limit = MAX_ENCODED_IMAGE_BYTES) {
  let c = canvas;
  for (let round = 0; round < 6; round += 1) {
    for (const [type, q] of [['image/png', undefined], ['image/webp', 0.9], ['image/webp', 0.7], ['image/jpeg', 0.8], ['image/jpeg', 0.6]]) {
      const blob = await new Promise((r) => c.toBlob(r, type, q));
      if (blob && blob.type === type && blob.size <= limit) return blob;
    }
    const next = document.createElement('canvas');
    next.width = Math.max(1, Math.round(c.width * 0.75)); next.height = Math.max(1, Math.round(c.height * 0.75));
    next.getContext('2d').drawImage(c, 0, 0, next.width, next.height);
    c = next;
  }
  throw new Error('This picture is too large to send. Try a smaller one.');
}

export function blobToBase64(blob) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).split(',')[1] ?? '');
    r.onerror = () => reject(r.error);
    r.readAsDataURL(blob);
  });
}

// source: {canvas} (a copy of the frozen frame) or null for a blank screen; `size` its CSS size.
export function openSketch(ctx, { session, source, size, onResult }) {
  const blank = !source;
  const w = size.w; const hgt = size.h;
  const sk = createSketch({ w, h: hgt });
  const base = h('canvas', { class: 'sketch-base', width: String(w), height: String(hgt) });
  const ink = h('canvas', { class: 'sketch-ink', width: String(w), height: String(hgt), 'aria-label': 'Sketch surface' });
  const bg = base.getContext('2d');
  bg.fillStyle = '#ffffff'; bg.fillRect(0, 0, w, hgt);
  if (source) bg.drawImage(source, 0, 0, w, hgt);
  let tool = 'pen';
  let live = null;
  const g = ink.getContext('2d');
  const redraw = () => { g.clearRect(0, 0, w, hgt); drawItems(g, sk.items); if (live) drawItems(g, [live]); };
  const pos = (e) => { const r = ink.getBoundingClientRect(); return [(e.clientX - r.left) * (w / r.width), (e.clientY - r.top) * (hgt / r.height)]; };
  ink.addEventListener('pointerdown', (e) => {
    e.preventDefault(); ink.setPointerCapture?.(e.pointerId);
    const [x, y] = pos(e);
    if (tool === 'text') {
      const text = window.prompt('Text to place');
      if (text) { sk.add({ tool: 'text', x, y, text }); redraw(); }
      return;
    }
    live = tool === 'pen' ? { tool, points: [[x, y]] } : { tool, x, y, w: 0, h: 0, x1: x, y1: y, x2: x, y2: y, o: [x, y] };
  });
  ink.addEventListener('pointermove', (e) => {
    if (!live) return;
    const [x, y] = pos(e);
    if (live.tool === 'pen') live.points.push([x, y]);
    else if (live.tool === 'rect') Object.assign(live, { x: Math.min(live.o[0], x), y: Math.min(live.o[1], y), w: Math.abs(x - live.o[0]), h: Math.abs(y - live.o[1]) });
    else Object.assign(live, { x2: x, y2: y });
    redraw();
  });
  const end = () => { if (live) { sk.add(live); live = null; redraw(); } };
  ink.addEventListener('pointerup', end);
  ink.addEventListener('pointercancel', () => { live = null; redraw(); });

  const tools = h('div', { class: 'sketch-tools', role: 'radiogroup', 'aria-label': 'Sketch tool' },
    [['pen', 'Pen', 'pencil'], ['rect', 'Rectangle', 'region'], ['arrow', 'Arrow', 'send'], ['text', 'Text', 'notes']].map(([id, label, ic]) =>
      h('button', { class: 'button button-plain button-small', type: 'button', role: 'radio', 'aria-checked': String(id === tool), dataset: { tool: id },
        onclick: (e) => { tool = id; for (const b of tools.children) b.setAttribute('aria-checked', String(b.dataset.tool === id)); e.currentTarget.blur(); } }, icon(ic, { size: 16 }), ` ${label}`)));
  const error = h('p', { class: 'form-error', role: 'alert' });
  const file = h('input', { type: 'file', accept: UPLOAD_TYPES.join(','), class: 'sr-only', 'aria-label': 'Upload image' });
  const done = h('button', { class: 'button button-primary', type: 'button' }, 'Use this picture');
  const body = h('div', { class: 'sketch' },
    h('p', { class: 'sketch-lede', text: blank ? `New screen sketch (${w} × ${hgt}): draw what you want.` : 'Draw on a frozen copy of the preview.' }),
    tools,
    h('div', { class: 'sketch-actions' },
      h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => { sk.undo(); redraw(); } }, icon('undo', { size: 16 }), ' Undo'),
      h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => { sk.clear(); redraw(); } }, 'Clear'),
      h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => file.click() }, icon('image', { size: 16 }), ' Upload image')),
    file, h('div', { class: 'sketch-stage' }, base, ink), error, h('div', { class: 'sheet-actions' }, done));
  const sheet = openSheet({ title: blank ? 'New screen' : 'Sketch', content: body, size: 'tall' });

  async function send(blob, extra) {
    const out = await ctx.design.upload({ session, media_type: blob.type, data: await blobToBase64(blob), ...extra });
    sheet.close();
    onResult(out, { blank });
  }
  done.addEventListener('click', async () => {
    done.disabled = true; error.textContent = '';
    try {
      const out = document.createElement('canvas');
      const fit = fitWithin(w, hgt, MAX_IMAGE_SIDE);
      out.width = fit.w; out.height = fit.h;
      const og = out.getContext('2d');
      og.drawImage(base, 0, 0, fit.w, fit.h); og.drawImage(ink, 0, 0, fit.w, fit.h);
      await send(await encodeUnder(out), { name: blank ? 'new-screen' : 'sketch', sketch: sk.serialise(), base: null, blank });
    } catch (err) { error.textContent = designErrorText(err); done.disabled = false; }
  });
  file.addEventListener('change', async () => {
    const f = file.files?.[0];
    if (!f) return;
    if (!UPLOAD_TYPES.includes(f.type)) { error.textContent = 'Choose a PNG, JPEG or WebP picture.'; return; }
    try {
      const bmp = await createImageBitmap(f);
      const fit = fitWithin(bmp.width, bmp.height, MAX_IMAGE_SIDE);
      const c = document.createElement('canvas'); c.width = fit.w; c.height = fit.h;
      c.getContext('2d').drawImage(bmp, 0, 0, fit.w, fit.h);
      const blob = f.size <= MAX_ENCODED_IMAGE_BYTES && fit.w === bmp.width ? f : await encodeUnder(c);
      await send(blob, { name: f.name.slice(0, 80), sketch: null, base: null, blank: false });
    } catch (err) { error.textContent = designErrorText(err); toast(error.textContent, { tone: 'red' }); }
  });
}
