// The design screen: #/design/<item>. Assembles the stage, notes, composer and
// the fixed chrome, and turns gestures into the design_* requests.
import { h } from '../../ui/dom.js';
import { emptyState } from '../../ui/components.js';
import { confirmSheet, openSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { designErrorText } from '../../core/design.js';
import { initialState, reduce, isLive } from './state.js';
import { createDesignStream } from './stream.js';
import { createStage } from './overlay.js';
import { createComposer } from './composer.js';
import { createNotesPanel } from './notes.js';
import { createInputBatcher, textEvent } from './input.js';
import { escapeKeyEvents, shortcutFor, isTextTarget } from './keymap.js';
import { createNoteOutbox } from './outbox.js';
import { noteFields } from './details.js';
import { startFinish, ledgerPanel } from './finish.js';
import { openSketch } from './sketch.js';
import { buildTopBar, buildModeBar, buildBottomBar, buildPhasePanel, buildStatusBar, openShortcuts } from './chrome.js';

const PHONE_PX = 640;
const WIDE_PX = 1024;
const SHEET_HEIGHTS = ['peek', 'half', 'full'];

export function mountDesign(view, ctx) {
  const item = ctx.route.param;
  const back = () => ctx.router.back(`#/item/${item}`);
  if (!/^qi_[A-Za-z0-9]+$/.test(item ?? '')) {
    view.append(emptyState({ title: 'Not an item', text: 'That link does not name a queue item.' }));
    return () => {};
  }
  const operator = ctx.canOperate;
  let state = initialState(item);
  let continued = false;
  let layout = 'desktop';
  let sheetHeight = 'peek';
  let pick = null; // where the open selection came from, for Wider / Narrower
  let tickets = 0;
  const outbox = createNoteOutbox({ item });

  const title = ctx.store.get().summaries?.find((s) => s.id === item)?.title ?? 'Design';
  const dispatch = (action) => {
    state = reduce(state, action);
    if (state.toasts.length) { for (const t of state.toasts) toast(t); state = reduce(state, { type: 'toasts_taken' }); }
    if (state.refresh) { state = { ...state, refresh: false }; stream.restart(); }
    if (state.dialog) showDialog();
    render();
  };
  const session = () => state.session?.id;
  const refuse = (err) => {
    if (err?.code === 'gone') dispatch({ type: 'event', event: { event: 'ended', reason: 'ended' } });
    else if (err?.code !== 'disconnected' && err?.code !== 'timeout') toast(designErrorText(err), { tone: 'red' });
  };

  // ---- parts ----
  const top = buildTopBar({
    title, onBack: back,
    onPreset: async (preset) => { dispatch({ type: 'preset', preset }); try { await ctx.design.viewport(session(), preset); } catch (e) { refuse(e); } },
    onReload: () => reload(),
    onFinish: () => startFinish(ctx, state, { onDone: ({ reload: r }) => r && stream.restart() }),
  });
  const modes = buildModeBar({ onMode: (m) => setMode(m), onNewScreen: () => newScreen() });
  const bottom = buildBottomBar({ onMode: (m) => setMode(m), onNotes: () => { sheetHeight = sheetHeight === 'peek' ? 'half' : 'peek'; render(); } });
  const phase = buildPhasePanel({
    cancelSession: async () => { if (await confirmSheet({ title: 'Cancel this session?', message: 'The item stops and the design session ends.', confirmLabel: 'Cancel session', cancelLabel: 'Keep waiting' })) { try { await ctx.cmds.cancel(item); } catch (e) { toast(designErrorText(e), { tone: 'red' }); } } },
    reload: () => reload(),
    tellAgent: async () => { try { const r = await ctx.design.tellAgent(session()); toast(r?.message ?? 'The agent was told again.'); } catch (e) { refuse(e); } },
    continueWithout: () => { continued = true; render(); },
  });
  const status = buildStatusBar();
  const batcher = createInputBatcher({ send: (events) => { if (session()) ctx.design.input(session(), events).catch(refuse); } });
  const stage = createStage({
    getMode: () => state.mode,
    onInput: (e) => batcher.push(e),
    onHover: hoverAt,
    onPick: pickAt,
    onRegion: regionAt,
    onCapture: (on) => { capture.hidden = !on; },
    onPin: (id) => notes.select(id),
  });
  const capture = h('div', { class: 'design-capture', role: 'status', hidden: true },
    h('span', { text: 'Typing into app · Esc to stop' }),
    h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => { for (const e of escapeKeyEvents()) batcher.push(e); batcher.flush(); } }, 'Send Esc to app'));
  const typeInput = h('input', { class: 'input input-inline', type: 'text', 'aria-label': 'Type text', placeholder: 'Type text', autocomplete: 'off', enterkeyhint: 'send' });
  const typeForm = h('form', { class: 'design-type', novalidate: true }, typeInput, h('button', { class: 'button button-plain button-small', type: 'submit' }, 'Send text'));
  typeForm.addEventListener('submit', (e) => { e.preventDefault(); if (typeInput.value) { batcher.push(textEvent(typeInput.value)); batcher.flush(); typeInput.value = ''; } });

  const composer = createComposer({
    onSend: sendNote,
    onCancel: () => { composer.hide(); stage.setSelection(null); dispatch({ type: 'selection', selection: null }); },
    onDepth: (depth) => reselect(depth),
  });
  const notes = createNotesPanel({ ctx, getState: () => state, onFocusNote: () => {}, onAddPage: () => composer.show(null, { keepText: true }) });
  const ledger = ledgerPanel(ctx, item);
  const sheetToggle = h('button', { class: 'design-sheet-grab', type: 'button', 'aria-label': 'Resize the notes sheet', onclick: () => { sheetHeight = SHEET_HEIGHTS[(SHEET_HEIGHTS.indexOf(sheetHeight) + 1) % 3]; render(); } });
  const side = h('aside', { class: 'design-side', 'aria-label': 'Notes' }, sheetToggle, notes.element, composer.element, ledger.element);
  const stageWrap = h('div', { class: 'design-stagewrap' }, stage.element, phase.element, capture);
  const main = h('div', { class: 'design-main' }, h('div', { class: 'design-stagecol' }, modes.element, stageWrap, typeForm, status.element), side);
  const root = h('div', { class: 'design', dataset: { layout, notes: sheetHeight } }, top.element, main, bottom.element);
  view.append(root);
  stage.layer.setAttribute('role', 'img');

  // ---- behaviour ----
  function setMode(mode) {
    if (mode === 'sketch') return openFrozenSketch();
    if (!operator || !isLive(state) || state.session?.state === 'finishing' && mode !== 'interact') return;
    stage.releaseCapture();
    stage.clearRegion();
    dispatch({ type: 'mode', mode });
    if (mode === 'region') stage.startKeyboardRegion();
    root.dataset.mode = mode;
  }
  function reload() { ctx.design.reload(session()).catch(refuse); }

  async function hoverAt(p) {
    const ticket = ++tickets;
    try {
      const r = await ctx.design.resolve(session(), p.x, p.y, 0, false);
      if (ticket === tickets && state.mode === 'comment') stage.setHover(r.target.box, r.label);
    } catch (e) { if (e.code === 'gone') refuse(e); }
  }
  async function pickAt(p) {
    pick = { ...p };
    stage.setHover(null);
    try { show(await ctx.design.resolve(session(), p.x, p.y, 0, true)); } catch (e) { refuse(e); }
  }
  async function reselect(depth) {
    if (!pick) return;
    try { show(await ctx.design.resolve(session(), pick.x, pick.y, depth, true), { keepDepth: true }); } catch (e) { refuse(e); }
  }
  async function regionAt(box) {
    pick = null;
    try { show(await ctx.design.region(session(), box)); } catch (e) { refuse(e); }
  }
  function show(resolved, opts) {
    dispatch({ type: 'selection', selection: { box: resolved.target.box, label: resolved.label } });
    composer.show(resolved, opts);
    if (layout !== 'desktop') { sheetHeight = 'half'; render(); }
  }
  function newScreen() { openSketch(ctx, { session: session(), source: null, size: { w: 390, h: 844 }, onResult: fromUpload }); }
  function openFrozenSketch() {
    if (!operator || !session()) return;
    if (!state.frame) return newScreen();
    const copy = document.createElement('canvas');
    copy.width = stage.canvas.width; copy.height = stage.canvas.height;
    copy.getContext('2d').drawImage(stage.canvas, 0, 0);
    openSketch(ctx, { session: session(), source: copy, size: { w: copy.width, h: copy.height }, onResult: fromUpload });
  }
  function fromUpload(out) {
    composer.show({ target: out.target ?? null, label: 'Your picture', asset: out.asset ?? out.id ?? null, data_url: out.data_url ?? null }, { keepText: true });
  }

  async function sendNote({ text, asset, sendPicture, sendDetails, details }) {
    const fields = noteFields({ session: session(), clientKey: undefined, text, asset, sendPicture, sendDetails, details });
    const entry = outbox.add({ ...fields, client_key: undefined, session: undefined });
    composer.setBusy(true);
    try {
      const note = await ctx.design.noteAdd({ ...entry, session: session() });
      outbox.remove(entry.client_key);
      dispatch({ type: 'event', event: { event: 'note', ...note } });
      composer.reset();
      stage.setSelection(null);
      notes.select(note.id);
    } catch (err) {
      if (err.code === 'disconnected' || err.code === 'timeout') {
        composer.reset();
        toast('Saved on this device. It will be sent when the connection is back.', { tone: 'amber' });
      } else { outbox.remove(entry.client_key); composer.setError(designErrorText(err)); refuse(err); }
    } finally { composer.setBusy(false); notes.setPending(outbox.pending()); }
  }
  let lastFlushAt = 0, lastFlushSession = null, lastFlushKind = null;
  const FLUSH_EVERY_MS = 5000;
  const flushOutbox = (kind) => {
    if (!session() || !isLive(state) || state.session.state === 'finishing') return;
    if (!outbox.pending().length) { lastFlushKind = kind; return; }
    // On reconnect or session change at once; otherwise at most once per few seconds.
    const fresh = session() !== lastFlushSession || (lastFlushKind === 'reconnecting' && kind !== 'reconnecting');
    const now = Date.now();
    lastFlushKind = kind;
    if (!fresh && now - lastFlushAt < FLUSH_EVERY_MS) return;
    lastFlushAt = now; lastFlushSession = session();
    outbox.flush((f) => ctx.design.noteAdd(f), session(), {
      onSent: (_e, note) => dispatch({ type: 'event', event: { event: 'note', ...note } }),
      onRefused: (_e, err) => toast(`A note typed offline was not sent: ${designErrorText(err)}`, { tone: 'red' }),
    }).then(() => notes.setPending(outbox.pending()));
  };

  let dialogOpen = false;
  function showDialog() {
    if (dialogOpen || !state.dialog) return;
    dialogOpen = true;
    const d = state.dialog;
    const input = d.kind === 'prompt' ? h('input', { class: 'input', type: 'text', value: d.defaultPrompt ?? '', 'aria-label': 'Answer' }) : null;
    let accepted = false;
    const answer = (accept) => { accepted = true; ctx.design.dialogAnswer(session(), accept, accept && input ? input.value : null).catch(refuse); sheet.close(); };
    const content = h('div', { class: 'confirm' }, h('p', { class: 'confirm-message', text: d.message }), input,
      h('button', { class: 'button button-primary button-block', type: 'button', onclick: () => answer(true) }, 'OK'),
      d.kind === 'alert' ? null : h('button', { class: 'button button-plain button-block', type: 'button', onclick: () => answer(false) }, 'Cancel'));
    const sheet = openSheet({ title: `The app says (${d.kind})`, content, onClose: () => {
      dialogOpen = false;
      if (!accepted && session()) ctx.design.dialogAnswer(session(), false, null).catch(() => {});
      dispatch({ type: 'dialog_closed' });
    } });
  }

  // ---- render ----
  function previewPath() {
    try { return state.session?.preview_url ? new URL(state.session.preview_url).pathname : ''; } catch { return ''; }
  }
  function render() {
    const features = ctx.store.get().designEnabled || ctx.store.get().connection.state !== 'open';
    const p = phase.update(state, { operator, features, continued });
    const live = isLive(state) && operator;
    const finishing = state.session?.state === 'finishing';
    top.update(state, { operator });
    modes.update(state.mode, Boolean(live));
    bottom.update(state.mode);
    status.update(state, { path: previewPath() });
    const showStage = live && (p.kind === 'live' || p.kind === 'finishing' || p.kind === 'preview_down' || p.kind === 'reconnecting');
    stage.element.hidden = !showStage;
    stage.layer.setAttribute('aria-label', status.frameText.textContent);
    stage.element.classList.toggle('is-dim', p.kind === 'preview_down' || p.kind === 'reconnecting');
    typeForm.hidden = !(showStage && state.mode === 'interact');
    side.hidden = !operator || !state.session;
    notes.element.hidden = false;
    ledger.element.hidden = !(state.session && (state.session.state === 'confirmed' || state.session.state === 'ended' || state.ended));
    composer.setDisabled(finishing, finishing ? 'Finishing: press Back to editing to change anything' : '');
    stage.setSelection(state.selection?.box ?? null, state.selection?.label ?? '');
    stage.setPins(state.notes.filter((n) => !n.read_only && n.target && n.kind !== 'revert' && n.status !== 'cancelled' && !n.undone_by)
      .map((n) => ({ id: n.id, number: n.number, box: n.target.box, label: n.text.slice(0, 60), tone: n.status === 'done' ? 'green' : n.status === 'needs_reply' ? 'amber' : n.status === 'failed' ? 'red' : 'accent' })));
    notes.render();
    root.dataset.layout = layout;
    root.dataset.notes = sheetHeight;
    root.dataset.phase = p.kind;
    root.dataset.preset = state.preset;
    root.dataset.mode = state.mode;
    if (state.loaded && ctx.canOperate && state.session?.state === 'live') flushOutbox(p.kind);
  }

  // ---- wiring ----
  const fit = () => {
    const w = window.innerWidth;
    layout = w < PHONE_PX ? 'phone' : w < WIDE_PX ? 'tablet' : 'desktop';
    stage.setPhone(layout === 'phone');
    root.dataset.layout = layout;
    stage.resize();
  };
  window.addEventListener('resize', fit);
  // The composer rides above the on-screen keyboard.
  const vv = window.visualViewport;
  const onViewport = () => {
    const covered = vv ? window.innerHeight - vv.height - vv.offsetTop : 0;
    root.style.setProperty('--keyboard', covered > 40 ? `${Math.round(covered)}px` : '0px');
  };
  vv?.addEventListener('resize', onViewport);
  fit();

  const stream = createDesignStream({
    ctx, item,
    onView: (kind, data) => dispatch(kind === 'design_state' ? { type: 'view', view: data } : { type: 'summary', summary: data }),
    onEvent: (e) => dispatch(e.type === 'connection' ? e : { type: 'event', event: e }),
    onFrame: (bitmap, meta) => stage.draw(bitmap, meta),
    onError: (err) => dispatch({ type: 'error', error: designErrorText(err) }),
  });
  const onKey = (e) => {
    if (document.getElementById('overlay')?.childElementCount) return;
    const action = shortcutFor(e, { capturing: stage.capturing, textFocus: isTextTarget(e.target) });
    if (!action || e.defaultPrevented) return;
    if (action.action === 'help') return openShortcuts();
    if (!operator) return;
    e.preventDefault();
    switch (action.action) {
      case 'escape': composer.hide(); stage.clearRegion(); stage.setHover(null); dispatch({ type: 'selection', selection: null }); break;
      case 'mode': setMode(action.value); break;
      case 'wider': reselect(1); break;
      case 'narrower': reselect(-1); break;
      case 'preset': top.element.querySelector(`[data-value="${action.value}"]`)?.click(); break;
      case 'fit_toggle': stage.toggleActual(); break;
      case 'reload': reload(); break;
      case 'next_note': case 'prev_note': {
        const list = state.notes; const at = list.findIndex((n) => n.id === notes.selected);
        const next = list[(at + (action.action === 'next_note' ? 1 : -1) + list.length) % list.length];
        if (next) notes.select(next.id);
        break;
      }
      default:
    }
  };
  document.addEventListener('keydown', onKey);
  const offFeatures = ctx.store.watch((s) => s.designEnabled, (on) => { if (on && !state.loaded) stream.start(); else render(); });
  const clock = setInterval(render, 30_000);
  const offConn = ctx.store.watch((s) => s.connection.state, () => render());
  render();

  return () => {
    stream.stop(); batcher.stop(); stage.destroy(); ledger.destroy();
    document.removeEventListener('keydown', onKey);
    window.removeEventListener('resize', fit);
    vv?.removeEventListener('resize', onViewport);
    clearInterval(clock); offFeatures(); offConn();
  };
}
