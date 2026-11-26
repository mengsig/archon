// The design screen: #/design/<item>. Assembles the stage, notes, composer and
// the fixed chrome, and turns gestures into the design_* requests.
import { h } from '../../ui/dom.js';
import { emptyState } from '../../ui/components.js';
import { confirmSheet, openSheet } from '../../ui/sheet.js';
import { toast } from '../../ui/toast.js';
import { designErrorText, isBackpressure, isLostInput } from '../../core/design.js';
import { initialState, reduce, isLive, currentPath, followsNewSession, presetMismatch, pointerBlockedText, PRESET_SIZE } from './state.js';
import { createDesignStream } from './stream.js';
import { createStage } from './overlay.js';
import { fileLimitMessage, uploadFiles } from './files.js';
import { showFileChooser, showDownload, showPicker, showClipboard } from './browser-prompts.js';
import { BEHIND, createBehindLine } from './behind.js';
import { createComposer } from './composer.js';
import { createNotesPanel, openUnsentDrafts } from './notes.js';
import { createInputBatcher, textEvent } from './input.js';
import { escapeKeyEvents, shortcutFor, isTextTarget, stepDepth } from './keymap.js';
import { blockedDraftReason, createNoteOutbox, newClientKey, retryableNoteError } from './outbox.js';
import { createThumbs } from './thumbs.js';
import { createTreePanel } from './tree.js';
import { createViewer } from './viewer.js';
import { buildChains, chainOfNote, olderInCode, followTarget, versionLabel, continuePlan, asideText, VERSION_ACTIONS } from './chains.js';
import { openCompare } from './compare.js';
import { noteFields } from './details.js';
import { startFinish, resumeFinish, ledgerPanel } from './finish.js';
import { openSketch } from './sketch.js';
import { sheetAfterCancel, readScroll, restoreScroll } from './sheet-state.js';
import { buildTopBar, buildModeBar, buildBottomBar, buildPhasePanel, buildStatusBar, openShortcuts } from './chrome.js';

const PHONE_PX = 640;
const WIDE_PX = 1024;
const SHEET_HEIGHTS = ['peek', 'half', 'full'];
const FOLLOW_EVERY_MS = 3000;
const FOLLOW_MAX_MS = 30_000;

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
  let scrollBeforeComposer = null; // page, stage column and sheet scroll when the composer opened, put back when it closes
  let pick = null; // where the open selection came from, for Wider / Narrower
  let depth = 0; // how many parents above the picked element; the one counter for keys and buttons
  let tickets = 0;
  let viewCurrent = false;
  let autoPreset = false;
  let presetSent = null; // `${session}|${preset}` of the last preset request, so a mismatch re-sends once
  let follower = null; // the poll that waits for the item's next session
  const outbox = createNoteOutbox({ item });
  let sendingNotes = 0;
  let blockedDrafts = [];
  const iterations = () => Boolean(ctx.store.get().designIterations);
  const thumbs = createThumbs(ctx, item);
  let tab = 'notes'; // 'notes' | 'tree'
  let viewing = null; // the version the comparison area shows, by note id
  let wasViewing = false;
  const continuedFrom = new Set(); // versions whose newer ones were already queued for undo

  const title = ctx.store.get().summaries?.find((s) => s.id === item)?.title ?? 'Design';
  const dispatch = (action) => {
    state = reduce(state, action);
    if (state.toasts.length) { for (const t of state.toasts) toast(t); state = reduce(state, { type: 'toasts_taken' }); }
    if (state.refresh) { state = { ...state, refresh: false }; stream.refresh(); }
    if (state.dialog) showDialog();
    if (state.browserEvents.length) {
      const events = state.browserEvents;
      state = reduce(state, { type: 'browser_events_taken' });
      for (const event of events) showBrowserEvent(event);
    }
    render();
  };
  const session = () => state.session?.id;
  // True from the click that opens the review sheet until the sheet is gone, so the top
  // bar offers Review / Back to editing only while the sheet is closed.
  let reviewOpen = false;
  async function reviewing(run) {
    let opened = false;
    reviewOpen = true;
    render();
    try {
      await run({
        onDone: ({ reload: r }) => r && stream.refresh(),
        onOpen: () => { opened = true; },
        onClose: () => { reviewOpen = false; render(); },
      });
    } finally {
      if (!opened) { reviewOpen = false; render(); }
    }
  }
  const refuse = (err) => {
    if (isBackpressure(err)) behind.notice(BEHIND.busy);
    else if (err?.code === 'gone') dispatch({ type: 'event', event: { event: 'ended', reason: 'ended' } });
    else if (err?.code === 'conflict') { toast(designErrorText(err), { tone: 'red' }); stream.refresh(); }
    else if (err?.code !== 'disconnected' && err?.code !== 'timeout') toast(designErrorText(err), { tone: 'red' });
  };

  // ---- parts ----
  const top = buildTopBar({
    title, onBack: back,
    onPreset: (preset) => setPreset(preset),
    onReload: () => reload(),
    onPreviewBack: () => { dispatch({ type: 'new_tab_dismissed' }); ctx.design.goBack(session()).catch(refuse); },
    onFinish: () => reviewing((hooks) => startFinish(ctx, state, hooks)),
    onReview: () => reviewing((hooks) => resumeFinish(ctx, state, hooks)),
    onBackToEditing: async () => {
      try { await ctx.design.finishCancel(session()); } catch (e) { refuse(e); }
      stream.refresh();
    },
  });
  const draftButton = h('button', { class: 'button button-plain button-small', type: 'button', hidden: true,
    'aria-haspopup': 'dialog', onclick: () => openUnsentDrafts(blockedDrafts) }, 'Unsent drafts');
  top.element.append(draftButton);
  const modes = buildModeBar({ onMode: (m) => setMode(m), onNewScreen: () => newScreen() });
  const bottom = buildBottomBar({ onMode: (m) => setMode(m), onNotes: () => { sheetHeight = sheetHeight === 'peek' ? 'half' : 'peek'; render(); } });
  const phase = buildPhasePanel({
    cancelSession: async () => { if (await confirmSheet({ title: 'Cancel this session?', message: 'The item stops and the design session ends.', confirmLabel: 'Cancel session', cancelLabel: 'Keep waiting' })) { try { await ctx.cmds.cancel(item); dispatch({ type: 'event', event: { event: 'ended', reason: 'cancelled' } }); stream.refresh(); } catch (e) { refuse(e); } } },
    reload: () => reload(),
    tellAgent: async () => { try { const r = await ctx.design.tellAgent(session()); toast(r?.message ?? 'The agent was told again.'); } catch (e) { refuse(e); } },
    continueWithout: () => { continued = true; render(); },
  });
  const status = buildStatusBar();
  const behind = createBehindLine({ show: (line) => status.setBusy(line) });
  const batcher = createInputBatcher({
    send: (events) => {
      const id = session();
      if (!id || !state.frame || !isLive(state) || state.connection !== 'open') return undefined;
      return ctx.design.input(id, events).catch((err) => {
        if (session() !== id) return;
        if (isBackpressure(err)) throw err; // archond ran none of it: the batcher retries
        if (isLostInput(err)) behind.notice(BEHIND.lost);
        else refuse(err);
      });
    },
    onOverflow: () => behind.notice(BEHIND.skipped),
    onBusy: (on) => behind.waiting(on),
  });
  const queueInput = (event) => state.frame && state.connection === 'open' && isLive(state) && batcher.push(event);
  const stage = createStage({
    getMode: () => state.mode,
    onInput: queueInput,
    onDrop: dropFiles,
    onHover: hoverAt,
    onPick: pickAt,
    onRegion: regionAt,
    onCapture: (on) => { capture.hidden = !on; },
    onPin: (id) => notes.select(id),
  });
  const capture = h('div', { class: 'design-capture', role: 'status', hidden: true },
    h('span', { text: 'Typing into app · Esc to stop' }),
    h('button', { class: 'button button-plain button-small', type: 'button', onclick: () => { for (const e of escapeKeyEvents()) queueInput(e); batcher.flush(); } }, 'Send Esc to app'));
  const typeInput = h('input', { class: 'input input-inline', type: 'text', 'aria-label': 'Type text', placeholder: 'Type text', autocomplete: 'off', enterkeyhint: 'send' });
  const typeForm = h('form', { class: 'design-type', novalidate: true }, typeInput, h('button', { class: 'button button-plain button-small', type: 'submit' }, 'Send text'));
  typeForm.addEventListener('submit', (e) => { e.preventDefault(); if (typeInput.value && queueInput(textEvent(typeInput.value))) { batcher.flush(); typeInput.value = ''; } });

  const composer = createComposer({
    onSend: sendNote,
    onCancel: () => { composer.hide(); stage.setSelection(null); dispatch({ type: 'selection', selection: null }); collapseSheet(true); },
    onDepth: (by) => widen(by),
    getNotes: () => state.notes, thumbs,
  });
  const notes = createNotesPanel({ ctx, getState: () => state, iterations, thumbs, onLink: (id) => { setTab('tree'); viewNode(id, { keep: true }); }, onFollowUp: sendFollowUp, onFocusNote: () => {}, onAddPage: () => { setTab('notes'); pick = null; depth = 0; stage.setSelection(null); dispatch({ type: 'selection', selection: null }); rememberScroll(); composer.show(null, { keepText: true }); raiseSheet(); } });
  // The decision ends the session at once; ask for it now and once more
  // shortly after, rather than waiting for the follower's next tick.
  let answeredTimer = null;
  const ledger = ledgerPanel(ctx, item, () => state, () => {
    stream.refresh();
    clearTimeout(answeredTimer);
    answeredTimer = setTimeout(() => stream.refresh(), 1500);
  });
  const sheetToggle = h('button', { class: 'design-sheet-grab', type: 'button', 'aria-label': 'Resize the notes sheet', onclick: () => { sheetHeight = SHEET_HEIGHTS[(SHEET_HEIGHTS.indexOf(sheetHeight) + 1) % 3]; render(); } });
  const tabButtons = ['notes', 'tree'].map((name) => h('button', { class: 'design-tab', type: 'button', role: 'tab', id: `design-tab-${name}`, 'aria-controls': `design-panel-${name}`, dataset: { tab: name }, onclick: () => setTab(name) }, name === 'notes' ? 'Notes' : 'Tree'));
  const tabs = h('div', { class: 'design-tabs', role: 'tablist', 'aria-label': 'Notes and tree', hidden: true }, tabButtons);
  const tree = createTreePanel({
    getState: () => state, thumbs, onSelect: (id) => viewNode(id), onLink: (id) => viewNode(id, { keep: true }), getViewing: () => viewing, isPhone: () => layout === 'phone',
    canAct: () => state.session?.state === 'live',
    actions: { back: () => { viewing = null; render(); }, compare: (n) => openCompare(ctx, n), followUp: (n) => followUpOn(n), continueFrom: (n) => continueFrom(n) },
  });
  notes.element.id = 'design-panel-notes';
  tree.element.id = 'design-panel-tree';
  tree.element.setAttribute('role', 'tabpanel');
  tree.element.setAttribute('aria-labelledby', 'design-tab-tree');
  tree.element.hidden = true;
  tabs.addEventListener('keydown', (event) => {
    const { key } = event;
    const at = tabButtons.indexOf(event.target);
    if (at === -1 || !['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(key)) return;
    event.preventDefault();
    const next = key === 'Home' ? 0 : key === 'End' ? tabButtons.length - 1 : (at + 1) % tabButtons.length;
    const button = tabButtons[next];
    setTab(button.dataset.tab);
    button.focus();
  });
  const viewer = createViewer({ ctx, onBack: () => { viewing = null; render(); }, onContinue: (n) => continueFrom(n), onFollowUp: (n) => followUpOn(n) });
  const side = h('aside', { class: 'design-side', 'aria-label': 'Notes' }, sheetToggle, tabs, notes.element, tree.element, composer.element);
  const stageWrap = h('div', { class: 'design-stagewrap' }, stage.element, phase.element, ledger.element, capture);
  const main = h('div', { class: 'design-main' }, h('div', { class: 'design-stagecol' }, viewer.element, modes.element, stageWrap, typeForm, status.element), side);
  const root = h('div', { class: 'design', dataset: { layout, notes: sheetHeight } }, top.element, main, bottom.element);
  view.append(root);
  stage.layer.setAttribute('role', 'img');

  // ---- iterations: tabs, viewing a version, follow-ups ----
  function setTab(name) { tab = name; render(); }
  function viewNode(id, { keep = false } = {}) {
    if (!state.notes.some((n) => n.id === id)) return;
    viewing = viewing === id && layout === 'phone' && !keep ? null : id;
    render();
    // On a phone the detail opens inline under the node: bring it into view.
    if (viewing && layout === 'phone') tree.element.querySelector('.tree-detail')?.scrollIntoView?.({ block: 'nearest' });
  }
  function followUpOn(note) {
    tab = 'notes';
    render();
    notes.openFollow(note.id);
    raiseSheet();
  }
  async function continueFrom(note) {
    const chain = chainOfNote(buildChains(state.notes), note.id);
    const { reverts: newer, aside } = continuePlan(chain, note);
    const ok = await confirmSheet({ title: `Continue from ${note.id}?`, message: `Archon reverses ${newer.length ? newer.join(', ') : 'the newer versions'} with new commits (nothing is erased) and marks ${newer.length > 1 ? 'them' : 'it'} Set aside. ${asideText(aside)}${aside.length ? ' ' : ''}Then you can follow up on ${note.id}.`, confirmLabel: VERSION_ACTIONS.continueFrom, cancelLabel: 'Keep as it is', tone: 'primary' });
    if (!ok) return;
    try {
      const view = await ctx.design.noteContinueFrom(item, note.id, { text: null, clientKey: newClientKey(), refs: [] });
      continuedFrom.add(note.id);
      if (view?.id) dispatch({ type: 'event', event: { event: 'note', ...view } });
      viewing = null;
      followUpOn(note);
    } catch (err) { refuse(err); }
  }
  // The follow-up box's Send: a plain note linked to its version, or (on an older version) the undo of the newer ones first.
  async function sendFollowUp({ target, text, refs }) {
    const chains = buildChains(state.notes);
    const chain = chainOfNote(chains, target);
    const base = chain?.versions.find((n) => n.id === target) ?? null;
    if (!base) throw new Error('That note is no longer here.');
    if (olderInCode(chain, base) && !continuedFrom.has(base.id)) {
      const { reverts: newer, aside } = continuePlan(chain, base);
      const ok = await confirmSheet({ title: `Follow up on ${base.id}?`, message: `${newer.join(', ')} will be undone first, with new commits, and set aside. ${asideText(aside)}${aside.length ? ' ' : ''}Then your follow-up on ${base.id} is sent.`, confirmLabel: 'Undo and follow up', cancelLabel: 'Keep as it is', tone: 'primary' });
      if (!ok) return false;
      const note = await ctx.design.noteContinueFrom(item, base.id, { text, clientKey: newClientKey(), refs });
      dispatch({ type: 'event', event: { event: 'note', ...note } });
      notes.select(note.id);
      return true;
    }
    const fields = noteFields({ session: session(), text, asset: null, sendPicture: false, sendDetails: false, details: '', refs, follows: base.id });
    const note = await submitNote(fields);
    if (note && session() === fields.session) notes.select(note.id);
    return true;
  }

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
  let presetAsks = 0; // numbers each preset request, so only the latest refusal rolls back
  async function setPreset(preset) {
    const ask = ++presetAsks;
    dispatch({ type: 'preset', preset });
    presetSent = `${session()}|${preset}`;
    try { await ctx.design.viewport(session(), preset); } catch (e) {
      // Refused (the app's dialog is open, say): back to the size the preview has.
      const latest = ask === presetAsks;
      if (latest) presetSent = null;
      dispatch({ type: 'preset_refused', latest });
      refuse(e);
    }
  }
  function reload() { dispatch({ type: 'new_tab_dismissed' }); ctx.design.reload(session()).catch(refuse); }

  // Taps resolve against the streamed size; until it matches the preset, say so instead of the daemon's "outside the preview".
  function waitingForSize() {
    const text = pointerBlockedText(state);
    if (text) toast(text);
    return Boolean(text);
  }
  // One hover read at a time; a pointer that moved meanwhile reads its latest place next.
  let hovering = false;
  let hoverNext = null;
  async function hoverAt(p) {
    if (pointerBlockedText(state)) return;
    if (hovering) { hoverNext = p; return; }
    hovering = true;
    const ticket = ++tickets;
    try {
      const r = await ctx.design.resolve(session(), p.x, p.y, 0, false);
      if (ticket === tickets && state.mode === 'comment') stage.setHover(r.target.box, r.label);
    } catch (e) { if (e.code === 'gone') refuse(e); } finally {
      hovering = false;
      const next = hoverNext;
      hoverNext = null;
      if (next && state.mode === 'comment') hoverAt(next);
    }
  }
  async function pickAt(p) {
    if (waitingForSize()) return;
    pick = { ...p };
    depth = 0;
    stage.setHover(null);
    try { show(await ctx.design.resolve(session(), p.x, p.y, 0, true)); } catch (e) { refuse(e); }
  }
  // Wider is +1 (the parent element), Narrower is -1, never below the picked element.
  async function widen(by) {
    if (!pick || !composer.open || !composer.resolved?.target || composer.resolved.target.region) return;
    const next = stepDepth(depth, by);
    if (next === depth) return;
    depth = next;
    try { show(await ctx.design.resolve(session(), pick.x, pick.y, depth, true)); } catch (e) { refuse(e); }
  }
  async function regionAt(box) {
    if (waitingForSize()) return;
    pick = null;
    depth = 0;
    try { show(await ctx.design.region(session(), box)); } catch (e) { refuse(e); }
  }
  function show(resolved, opts) {
    dispatch({ type: 'selection', selection: { box: resolved.target.box, label: resolved.label } });
    rememberScroll();
    composer.show(resolved, opts);
    raiseSheet();
  }
  // The composer opens at the top of a half-raised sheet.
  function raiseSheet() {
    if (layout === 'desktop') return;
    sheetHeight = 'half';
    render();
    side.scrollTop = 0;
  }
  function rememberScroll() { if (layout !== 'desktop' && !composer.open) scrollBeforeComposer = readScroll([main, side]); }
  // Cancel and Escape: lower the sheet and put the page back so the stage is
  // where it was. With no composer open there is nothing to undo.
  function collapseSheet(composerWasOpen) {
    if (layout === 'desktop' || !composerWasOpen) return;
    sheetHeight = sheetAfterCancel(layout, sheetHeight, composerWasOpen);
    render();
    if (scrollBeforeComposer) restoreScroll(scrollBeforeComposer);
    scrollBeforeComposer = null;
  }
  function newScreen() {
    const size = state.frame ? { w: state.frame.w, h: state.frame.h } : (PRESET_SIZE[state.preset] ?? PRESET_SIZE.desktop);
    openSketch(ctx, { session: session(), source: null, size, onResult: fromUpload });
  }
  function openFrozenSketch() {
    if (!operator || !session()) return;
    if (!state.frame) return newScreen();
    const copy = document.createElement('canvas');
    copy.width = stage.canvas.width; copy.height = stage.canvas.height;
    copy.getContext('2d').drawImage(stage.canvas, 0, 0);
    openSketch(ctx, { session: session(), source: copy, size: { w: copy.width, h: copy.height }, onResult: fromUpload });
  }
  function fromUpload(out, { blank = false } = {}) {
    pick = null;
    depth = 0;
    const label = blank ? 'New screen sketch' : 'Sketch over the preview';
    rememberScroll();
    composer.show({ target: out.target ?? null, label, details: `${label}, drawn by the operator.`, asset: out.asset ?? out.id ?? null, data_url: out.data_url ?? null }, { keepText: true });
    raiseSheet();
  }

  async function sendNote({ text, asset, sendPicture, sendDetails, details, refs }) {
    if (composer.busy) return;
    const fields = noteFields({ session: session(), text, asset, sendPicture, sendDetails, details, refs: iterations() ? refs : [] });
    composer.setBusy(true);
    try {
      const note = await submitNote(fields);
      composer.reset();
      stage.setSelection(null);
      dispatch({ type: 'selection', selection: null });
      collapseSheet(true);
      if (note && session() === fields.session) notes.select(note.id);
    } catch (err) {
      composer.setError(designErrorText(err));
    } finally { composer.setBusy(false); }
  }

  const canSendDraft = (id) => viewCurrent && state.connection === 'open' &&
    state.session?.state === 'live' && session() === id;
  function savedDraft(entry) {
    if (!outbox.pending().some((draft) => draft.client_key === entry.client_key)) {
      throw new Error('This device could not save the draft. Keep this note open and try again.');
    }
    toast('Saved on this device. It can only be sent to its original design session.', { tone: 'amber' });
  }

  async function submitNote(fields) {
    const entry = outbox.add(fields);
    if (!canSendDraft(entry.session)) {
      renderDrafts();
      savedDraft(entry);
      return null;
    }
    sendingNotes += 1;
    try {
      const note = await ctx.design.noteAdd(entry);
      outbox.remove(entry.client_key);
      if (session() === entry.session) dispatch({ type: 'event', event: { event: 'note', ...note } });
      return note;
    } catch (err) {
      if (err?.code === 'gone') {
        outbox.markEnded(entry.client_key);
        if (session() === entry.session) refuse(err);
      } else if (!retryableNoteError(err)) {
        outbox.remove(entry.client_key);
        throw err;
      }
      savedDraft(entry);
      return null;
    } finally {
      sendingNotes -= 1;
      renderDrafts();
    }
  }

  function renderDrafts() {
    const waiting = [];
    const blocked = [];
    const phase = state.session?.state ?? (viewCurrent ? 'ended' : null);
    for (const entry of operator ? outbox.pending() : []) {
      const reason = blockedDraftReason(entry, session(), phase);
      if (reason) blocked.push({ entry, reason });
      else waiting.push(entry);
    }
    blockedDrafts = blocked;
    draftButton.hidden = blocked.length === 0;
    notes.setPending(waiting);
    return waiting;
  }
  let lastFlushAt = -Infinity;
  let lastFlushSession = null;
  const FLUSH_EVERY_MS = 5000;
  const flushOutbox = (waiting) => {
    const id = session();
    if (!canSendDraft(id) || sendingNotes || !waiting.length) return;
    // On reconnect or session change at once; otherwise at most once per few seconds.
    const fresh = id !== lastFlushSession;
    const now = Date.now();
    if (!fresh && now - lastFlushAt < FLUSH_EVERY_MS) return;
    lastFlushAt = now; lastFlushSession = id;
    outbox.flush((f) => ctx.design.noteAdd(f), id, {
      canSend: () => canSendDraft(id),
      onSent: (_e, note) => { if (session() === id) dispatch({ type: 'event', event: { event: 'note', ...note } }); },
      onRefused: (_e, err) => {
        if (err?.code === 'gone' && session() === id) refuse(err);
        toast(`A note typed offline was not sent: ${designErrorText(err)}`, { tone: 'red' });
      },
    }).then(renderDrafts);
  };

  function showBrowserEvent(event) {
    const id = session();
    if (!id) return;
    if (event.event === 'file_chooser') showFileChooser(ctx.design, id, event, refuse);
    else if (event.event === 'download') showDownload(ctx.design, id, event, refuse);
    else if (event.event === 'picker') showPicker(ctx.design, id, event, refuse);
    else if (event.event === 'clipboard') showClipboard(event.text);
  }

  async function dropFiles(files, point) {
    const problem = fileLimitMessage(files);
    if (problem) { toast(problem, { tone: 'amber' }); return; }
    if (!files.length) return;
    const id = session();
    if (!id || !isLive(state)) return;
    try {
      const transfer = await uploadFiles(ctx.design, id, files);
      await ctx.design.fileDrop(id, transfer, point.x, point.y);
    } catch (error) { refuse(error); }
  }

  // A text input drops line breaks, so a multi-line suggested answer gets a text area.
  function promptInput(value) {
    const multiline = value.includes('\n');
    const input = h(multiline ? 'textarea' : 'input', { class: 'input', 'aria-label': 'Answer', ...(multiline ? { rows: 4 } : { type: 'text' }) });
    input.value = value;
    return input;
  }

  let dialogOpen = false;
  function showDialog() {
    if (dialogOpen || !state.dialog) return;
    dialogOpen = true;
    const d = state.dialog;
    const input = d.kind === 'prompt' ? promptInput(d.defaultPrompt ?? '') : null;
    let accepted = false;
    const answer = (accept) => { accepted = true; ctx.design.dialogAnswer(session(), accept, accept && input ? input.value : null).catch(refuse); sheet.close(); };
    const content = h('div', { class: 'confirm' }, h('p', { class: 'confirm-message', text: d.message }), input,
      h('button', { class: 'button button-primary button-block', type: 'button', onclick: () => answer(true) }, 'OK'),
      d.kind === 'alert' ? null : h('button', { class: 'button button-plain button-block', type: 'button', onclick: () => answer(false) }, 'Cancel'));
    const sheet = openSheet({ title: `The app says (${d.kind})`, content, onClose: () => {
      dialogOpen = false;
      if (!accepted && session()) ctx.design.dialogAnswer(session(), false, null).catch(() => {});
      dispatch({ type: 'dialog_closed', dialog: d });
    } });
  }

  // ---- render ----
  const previewPath = () => currentPath(state);
  function render() {
    const features = ctx.store.get().designEnabled || ctx.store.get().connection.state !== 'open';
    const p = phase.update(state, { operator, features, continued });
    const live = isLive(state) && operator;
    const finishing = state.session?.state === 'finishing';
    top.update(state, { operator, reviewOpen });
    modes.update(state.mode, Boolean(live));
    // Once confirmed, nothing is edited: the ledger and its answer fill the screen.
    const atEnd = p.kind === 'confirmed' || p.kind === 'ended';
    modes.element.hidden = atEnd;
    bottom.element.hidden = atEnd;
    bottom.update(state.mode);
    status.update(state, { path: previewPath() });
    const showStage = live && (p.kind === 'live' || p.kind === 'finishing' || p.kind === 'preview_down' || p.kind === 'reconnecting');
    stage.element.hidden = !showStage;
    stage.layer.setAttribute('aria-label', status.frameText.textContent);
    stage.element.classList.toggle('is-dim', p.kind === 'preview_down' || p.kind === 'reconnecting');
    // A picture at the old size is not shown while the preview switches size;
    // with the preview down or reconnecting the dimmed last picture says so.
    const switching = (p.kind === 'live' || p.kind === 'finishing') && presetMismatch(state);
    stage.setSwitching(showStage && switching ? PRESET_SIZE[state.preset] : null, pointerBlockedText(state));
    typeForm.hidden = !(showStage && state.mode === 'interact');
    side.hidden = !operator || !state.session;
    const iter = iterations() && operator;
    tabs.hidden = !iter;
    const treeTab = iter && tab === 'tree';
    for (const b of tabButtons) { const on = b.dataset.tab === (treeTab ? 'tree' : 'notes'); b.setAttribute('aria-selected', String(on)); b.tabIndex = on ? 0 : -1; }
    if (iter) {
      notes.element.setAttribute('role', 'tabpanel');
      notes.element.setAttribute('aria-labelledby', 'design-tab-notes');
    } else {
      notes.element.removeAttribute('role');
      notes.element.setAttribute('aria-labelledby', 'design-notes-title');
    }
    notes.element.hidden = treeTab;
    tree.element.hidden = !treeTab;
    composer.setRefsEnabled(iterations());
    if (viewing && !state.notes.some((n) => n.id === viewing)) viewing = null;
    const shown = iter && viewing ? state.notes.find((n) => n.id === viewing) : null;
    const chain = shown ? chainOfNote(buildChains(state.notes), shown.id) : null;
    const wide = layout !== 'phone';
    if (shown && chain && wide) viewer.show(chain, shown, { act: state.session?.state === 'live' }); else viewer.hide();
    const nowViewing = Boolean(shown && wide);
    root.dataset.viewing = String(nowViewing);
    if (wasViewing && !nowViewing) requestAnimationFrame(() => stage.resize());
    wasViewing = nowViewing;
    if (treeTab) tree.render();
    ledger.update();
    composer.setDisabled(finishing, finishing ? 'Finishing: press Back to editing to change anything' : '');
    stage.setSelection(state.selection?.box ?? null, state.selection?.label ?? '');
    stage.setPins(state.notes.filter((n) => !n.read_only && n.target && (n.target.path ?? '/') === (previewPath() || '/') && n.kind !== 'revert' && n.status !== 'cancelled' && !n.undone_by)
      .map((n) => ({ id: n.id, number: n.number, box: n.target.box, label: n.text.slice(0, 60), tone: n.status === 'done' ? 'green' : n.status === 'needs_reply' ? 'amber' : n.status === 'failed' ? 'red' : 'accent' })));
    notes.render();
    root.dataset.layout = layout;
    root.dataset.notes = sheetHeight;
    root.dataset.phase = p.kind;
    root.dataset.preset = state.preset;
    root.dataset.mode = state.mode;
    const waiting = renderDrafts();
    if (operator) flushOutbox(waiting);
    // The daemon may not have applied the preset the session says it has: a frame at another size re-sends it, once.
    if (operator && state.session?.state === 'live' && presetMismatch(state) && presetSent !== `${session()}|${state.preset}`) {
      presetSent = `${session()}|${state.preset}`;
      ctx.design.viewport(session(), state.preset).catch(refuse);
    }
    syncFollower();
    // A phone starts on the Phone preset, once.
    if (!autoPreset && layout === 'phone' && operator && state.session?.state === 'live') {
      autoPreset = true;
      if (state.preset !== 'phone') setPreset('phone');
    }
  }

  // While the gate waits or changes were requested, the next session is only found by asking.
  function syncFollower() {
    const want = operator && followsNewSession(state);
    if (want && !follower) {
      // Quick at first (the next session usually opens within seconds), then
      // backing off, so a screen left open on a run that never designs again
      // asks only every half minute.
      let wait = FOLLOW_EVERY_MS;
      const tick = () => {
        stream.refresh();
        if (follower === null) return; // stopped meanwhile
        wait = Math.min(wait * 1.5, FOLLOW_MAX_MS);
        follower = setTimeout(tick, wait);
      };
      follower = setTimeout(tick, wait);
    } else if (!want && follower) { clearTimeout(follower); follower = null; }
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
    onView: (kind, data) => {
      viewCurrent = true;
      dispatch(kind === 'design_state' ? { type: 'view', view: data } : { type: 'summary', summary: data });
    },
    onEvent: (e) => dispatch(e.type === 'connection' ? e : { type: 'event', event: e }),
    onFrame: (bitmap, meta) => stage.draw(bitmap, meta),
    onReset: () => {
      viewCurrent = false;
      lastFlushAt = -Infinity;
      tickets += 1;
      batcher.stop();
      stage.clearFrame();
      state = reduce(state, { type: 'frame_reset' });
    },
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
      case 'escape': { const wasOpen = composer.open; composer.hide(); stage.clearRegion(); stage.setHover(null); dispatch({ type: 'selection', selection: null }); collapseSheet(wasOpen); break; }
      case 'mode': setMode(action.value); break;
      case 'wider': widen(1); break;
      case 'narrower': widen(-1); break;
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
  // The item's own state, so an approved design shows when the item is done.
  const itemStateOf = (s) => s.summaries?.find((x) => x.id === item)?.state?.state ?? null;
  const offItem = ctx.store.watch(itemStateOf, (st) => dispatch({ type: 'item_state', state: st }));
  render();

  return () => {
    stream.stop(); batcher.stop(); behind.stop(); stage.destroy(); ledger.destroy();
    document.removeEventListener('keydown', onKey);
    window.removeEventListener('resize', fit);
    vv?.removeEventListener('resize', onViewport);
    clearInterval(clock); clearTimeout(follower); clearTimeout(answeredTimer); follower = null; offFeatures(); offConn(); offItem();
  };
}
