// A stand-in host for the design screen: serves the shipped web app, answers the
// session API, and plays the daemon over a routed WebSocket. Its "agent" restyles
// the fixture app (fixtures/design-app) and its "browser" is a second Playwright
// page whose screenshots are the frames. No archond is needed.
import { createServer } from 'node:http';
import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join, resolve, dirname, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { startFixtureApp } from '../fixtures/design-app/server.js';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png', '.webmanifest': 'application/manifest+json' };
const PIXEL = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==';
export const ITEM = 'qi_FIXTURE';
const CSS_BASE = readFileSync(join(ROOT, 'tests/fixtures/design-app/style.css'), 'utf8');

// A finished note as the daemon reports it (for seeding a session); `fields` override.
export function seedNote(fields) {
  const at = new Date().toISOString();
  const number = fields.number ?? Number(String(fields.id).slice(1));
  return { item: ITEM, id: `D${number}`, number, session: 'ds_FIXTURE', kind: 'change', text: '', details_text: null, send_picture: false, target: null, viewport: { w: 800, h: 500, preset: 'desktop' },
    status: 'done', fail_reason: null, base_commit: 'base0001', commit: `c${number}0000abcdef`, no_change: false, reverts: null, resent_from: null, superseded_by: null, thread: [], delivery: 0,
    before_asset: `da_b${number}`, after_asset: `da_a${number}`, created_at: at, updated_at: at, follows: null, refs: [], ...fields };
}

export async function startHarness(browser, { role = 'operator', questions = [], summaries = [], notes = [], iterations = false } = {}) {
  const fixtureApp = await startFixtureApp();
  const appPage = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage();
  await appPage.goto(fixtureApp.url);
  const server = createServer((req, res) => {
    const path = req.url.split('?')[0];
    if (path === '/api/session') return json(res, { device: { id: 'dev1', name: 'Test', role }, protocol_version: iterations ? 26 : 25, daemon_version: 'test' });
    if (path === '/api/health') return json(res, { ok: true });
    const file = join(ROOT, path === '/' ? 'index.html' : path.slice(1));
    if (!file.startsWith(ROOT) || /\/(tests|dev|node_modules)\//.test(file) || !existsSync(file)) { res.writeHead(404).end(); return; }
    res.writeHead(200, { 'Content-Type': TYPES[extname(file)] ?? 'application/octet-stream', 'Cache-Control': 'no-store' }).end(readFileSync(file));
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const daemon = createDaemon({ appPage, fixtureApp, role });
  daemon.state.iterations = iterations;
  daemon.state.questions.push(...questions);
  daemon.state.summaries.push(...summaries);
  daemon.state.notes.push(...notes);
  return {
    baseURL: `http://127.0.0.1:${server.address().port}`,
    daemon,
    appPage,
    install: (page) => page.routeWebSocket(/\/ws$/, (ws) => daemon.connect(ws)),
    stop: async () => { daemon.stop(); await appPage.context().close(); await fixtureApp.stop(); server.close(); writeFileSync(fixtureApp.css, CSS_BASE); },
  };
}

const PRESETS = { desktop: { width: 1440, height: 900 }, tablet: { width: 834, height: 1112 }, phone: { width: 390, height: 844 } };
const json = (res, body) => res.writeHead(200, { 'Content-Type': 'application/json' }).end(JSON.stringify(body));

function createDaemon({ appPage, fixtureApp, role }) {
  const state = { notes: [], seq: 0, ended: false, paused: false, sessionState: 'live', requests: [], connections: new Set(), timer: null, version: 1, finishing: false, questions: [], summaries: [], sessionPatch: {}, sessionId: 'ds_FIXTURE', endReason: null };
  const now = () => new Date().toISOString();
  const session = () => ({ id: state.sessionId, item: ITEM, state: state.sessionState, end_reason: state.endReason, degraded: false, note_version: state.version, viewport_preset: 'desktop', preview_url: 'http://127.0.0.1:5173/', sandbox_off: false, ...state.sessionPatch });
  const undoneBy = (n) => state.notes.find((x) => x.reverts === n.id && x.status === 'done')?.id ?? null;
  const rootOf = (n) => { let c = n; for (let i = 0; i < 50 && c.follows && state.notes.find((x) => x.id === c.follows); i += 1) c = state.notes.find((x) => x.id === c.follows); return c.id; };
  const chainOf = (n) => (n.kind === 'revert' ? null : (n.chain ?? rootOf(n)));
  // The iteration fields of protocol 26, derived the way the daemon does.
  const iter = (n) => {
    if (!state.iterations) return {};
    const chain = chainOf(n);
    const mates = chain ? state.notes.filter((x) => chainOf(x) === chain).sort((a, b) => a.number - b.number) : [];
    return { follows: n.follows ?? null, refs: n.refs ?? [], chain, version: chain ? mates.findIndex((x) => x.id === n.id) + 1 : 0, versions: mates.length, set_aside: Boolean(undoneBy(n)) };
  };
  const view = (n) => ({ ...n, undone_by: undoneBy(n), read_only: false, approved: false, active: true, ...iter(n) });
  const nextNumber = () => Math.max(0, ...state.notes.map((n) => n.number)) + 1;
  const push = (ws, event) => ws.send(JSON.stringify({ frame: 'design', sub: 1, event }));
  const broadcast = (event) => state.connections.forEach((c) => c.subscribed && push(c.ws, event));
  const reply = (ws, id, reply, data) => ws.send(JSON.stringify({ frame: 'reply', id, result: { Ok: { reply, data } } }));
  const refuse = (ws, id, code, message) => ws.send(JSON.stringify({ frame: 'reply', id, result: { Err: { code, message } } }));

  async function sendFrame(conn) {
    if (state.ended || state.paused || !conn.subscribed) return;
    const jpeg = await appPage.screenshot({ type: 'jpeg', quality: 60 }).catch(() => null);
    if (!jpeg || state.ended) return;
    const { width, height } = appPage.viewportSize();
    push(conn.ws, { event: 'frame', seq: ++state.seq, jpeg_b64: jpeg.toString('base64'), w: width, h: height, at: now() });
  }
  async function target() {
    // The protocol's Rect names its size w and h, not Playwright's width and height.
    const { x, y, width: w, height: h } = await appPage.locator('#cta').boundingBox();
    return { path: '/', selector: 'main > button#cta.primary', role_name: 'button "Buy now"', component: null, box: { x, y, w, h }, viewport: { w: 800, h: 500, preset: 'desktop' }, styles: { color: 'rgb(255, 255, 255)', display: 'block' }, region: false };
  }
  const note = (fields) => ({ item: ITEM, id: `D${fields.number}`, session: 'ds_FIXTURE', kind: 'change', text: '', details_text: null, send_picture: true, target: null, viewport: { w: 800, h: 500, preset: 'desktop' },
    status: 'queued', fail_reason: null, base_commit: 'base0001', commit: null, no_change: false, reverts: null, resent_from: null, superseded_by: null, thread: [], delivery: 0,
    before_asset: null, after_asset: null, created_at: now(), updated_at: now(), ...fields });
  const setNote = (n, patch) => { Object.assign(n, patch, { updated_at: now() }); state.version += 1; broadcast({ event: 'note', ...view(n) }); };
  const recolor = async (color) => { writeFileSync(fixtureApp.css, `${CSS_BASE}\n.primary { background: ${color}; }\n`); await appPage.waitForFunction((c) => getComputedStyle(document.querySelector('#cta')).backgroundColor === c, color === '#1f8a3b' ? 'rgb(31, 138, 59)' : 'rgb(40, 93, 206)'); };

  async function handle(conn, msg) {
    const { ws } = conn;
    const { req, cmd } = msg;
    state.requests.push(msg);
    if (cmd === 'hello') return reply(ws, req, 'welcome', { protocol_version: state.iterations ? 26 : 25, features: state.iterations ? ['design_sessions', 'design_iterations'] : ['design_sessions'], device: { id: 'dev1', name: 'Test', role } });
    if (cmd === 'feed_history') return reply(ws, req, 'feed', []);
    if (cmd === 'list_item_summaries') return reply(ws, req, 'item_summaries', state.summaries);
    if (cmd === 'list_questions') return reply(ws, req, 'questions', state.questions);
    if (cmd === 'answer') {
      const q = state.questions.find((x) => x.id === msg.question);
      if (!q) return refuse(ws, req, 'not_found', 'no such question');
      q.answer = msg.text; q.answered_at = now();
      return reply(ws, req, 'question', q);
    }
    if (cmd === 'repo_list') return reply(ws, req, 'repos', []);
    if (cmd === 'list_pipelines') return reply(ws, req, 'catalog', { pipelines: [] });
    if (cmd === 'enqueue') return reply(ws, req, 'item', { id: 'qi_NEW', title: msg.title });
    if (cmd?.startsWith('subscribe_') && cmd !== 'subscribe_design') return reply(ws, req, 'subscribed', { id: 9, last_seq: 0 });
    if (!cmd?.startsWith('design_') && cmd !== 'subscribe_design') return refuse(ws, req, 'unsupported', `${cmd} is not part of this stand-in`);
    if (cmd === 'design_get') {
      if (role !== 'operator') return reply(ws, req, 'design_summary', { state: 'live', note_count: state.notes.length });
      return reply(ws, req, 'design_state', { item: ITEM, session: session(), notes: state.notes.map(view), browser: { status: 'running' }, finish_blocker: null, images_used: 1, images_max: 128, stage_deadline: null });
    }
    if (state.ended) return refuse(ws, req, 'gone', 'This design session has ended.');
    switch (cmd) {
      case 'design_viewport': {
        const size = PRESETS[msg.preset];
        state.sessionPatch.viewport_preset = msg.preset;
        if (size) await appPage.setViewportSize(size);
        return reply(ws, req, 'design_done', null);
      }
      case 'subscribe_design': conn.subscribed = true; reply(ws, req, 'subscribed', { id: 1, last_seq: 0 }); return void setTimeout(() => sendFrame(conn), 50);
      case 'design_frame_ack': reply(ws, req, 'design_done', null); return void setTimeout(() => sendFrame(conn), 120);
      case 'design_resolve': return reply(ws, req, 'design_resolved', { target: await target(), label: 'button "Buy now"', asset: 'da_pending', data_url: PIXEL });
      case 'design_region': return reply(ws, req, 'design_resolved', { target: { ...(await target()), selector: null, role_name: null, box: msg.box, region: true }, label: 'Region', asset: 'da_region', data_url: PIXEL });
      case 'design_note_add': {
        if (state.sessionState === 'finishing') return refuse(ws, req, 'finishing', 'finishing');
        // Resolve first, then look up and insert with no await between, as
        // archond's one transaction does: a resent client_key finds its note.
        const resolved = await target();
        const known = state.notes.find((n) => n.client_key === msg.client_key);
        if (known) return reply(ws, req, 'design_note', view(known));
        const n = note({ number: nextNumber(), client_key: msg.client_key, text: msg.text, details_text: msg.details_text, send_picture: msg.send_picture, target: resolved, follows: msg.follows ?? null, refs: msg.refs ?? [] });
        state.notes.unshift(n);
        reply(ws, req, 'design_note', view(n));
        setTimeout(async () => {
          setNote(n, { status: 'working' });
          await recolor('#1f8a3b');
          setNote(n, { status: 'done', commit: 'abc1234def', before_asset: 'da_b', after_asset: 'da_a', thread: [{ at: now(), from: 'agent', text: 'Made the button green.' }] });
        }, 200);
        return;
      }
      // As archond: revert every newer version of the chain that is in the code (newest first), then the follow-up.
      case 'design_note_continue_from': {
        const base = state.notes.find((n) => n.id === msg.id);
        if (!base) return refuse(ws, req, 'not_found', 'no such note');
        if (state.refuseContinue) return refuse(ws, req, 'invalid_argument', state.refuseContinue);
        const known = msg.client_key && state.notes.find((n) => n.client_key === msg.client_key);
        if (known) return reply(ws, req, 'design_note', view(known));
        const chain = chainOf(base);
        const newer = state.notes.filter((n) => chainOf(n) === chain && n.number > base.number && n.status === 'done' && !undoneBy(n)).sort((a, b) => b.number - a.number);
        const reverts = newer.map((orig) => { const r = note({ number: nextNumber(), kind: 'revert', reverts: orig.id, status: 'queued', client_key: `rev-${orig.id}`, text: '' }); state.notes.unshift(r); return r; });
        let follow = null;
        if (msg.text) { follow = note({ number: nextNumber(), client_key: msg.client_key, text: msg.text, follows: base.id, refs: msg.refs ?? [], target: null }); state.notes.unshift(follow); }
        reply(ws, req, 'design_note', view(follow ?? base));
        setTimeout(async () => {
          for (const r of reverts) {
            setNote(r, { status: 'working' });
            if (state.conflictOn === r.reverts) { setNote(r, { status: 'failed', fail_reason: 'undo_conflict', thread: [{ at: now(), from: 'archon', text: 'Can’t undo automatically, later changes touch the same code; add a note describing what to change back.' }] }); if (follow) setNote(follow, { status: 'cancelled' }); return; }
            setNote(r, { status: 'done', commit: `rev${r.number}0000` });
            setNote(state.notes.find((n) => n.id === r.reverts), {});
          }
          if (follow) { setNote(follow, { status: 'working' }); setNote(follow, { status: 'done', commit: `f${follow.number}0000abcdef`, before_asset: 'da_b', after_asset: 'da_a', thread: [{ at: now(), from: 'agent', text: 'Follow-up done.' }] }); }
        }, 100);
        return;
      }
      case 'design_upload': return reply(ws, req, 'design_resolved', { target: null, label: 'Your picture', asset: 'da_up', data_url: PIXEL });
      case 'design_asset': return reply(ws, req, 'design_asset', { id: msg.id, data_url: PIXEL, width: 1, height: 1, caption: 'captured from the preview at commit abc1234, 14:32' });
      case 'design_note_undo': {
        const original = state.notes.find((n) => n.id === msg.id);
        const revert = note({ number: nextNumber(), kind: 'revert', reverts: original.id, status: 'working', client_key: 'rev', text: '' });
        state.notes.unshift(revert);
        reply(ws, req, 'design_note', view(revert));
        setTimeout(async () => { await recolor('#285dce'); setNote(revert, { status: 'done', commit: 'rev00001' }); setNote(original, {}); }, 100);
        return;
      }
      case 'design_finish_capture':
        state.sessionState = 'finishing'; state.version += 1;
        broadcast({ event: 'session', state: 'finishing', degraded: false, preview_path: '/', commit: 'abc1234def', browser: { status: 'running' }, note_version: state.version });
        return reply(ws, req, 'design_finish', { capture: 'cap1', head: 'abc1234def', note_version: state.version, no_change_only: false,
          cards: state.notes.filter((n) => n.status === 'done' && n.kind !== 'revert' && !view(n).undone_by && (!state.iterations || !state.notes.some((m) => m.status === 'done' && !undoneBy(m) && chainOf(m) === chainOf(n) && m.number > n.number))).map((n) => ({ note: n.id, text: n.text, history: state.iterations ? state.notes.filter((m) => chainOf(m) === chainOf(n) && m.number < n.number).sort((a, b) => a.number - b.number).map((m) => ({ note: m.id, text: m.text, set_aside: Boolean(undoneBy(m)) })) : undefined, asset: 'da_f', data_url: PIXEL, caption: 'captured from the preview at commit abc1234', include: true, superseded_by: null, not_found: false, left_history: false })) });
      case 'design_finish_cancel': state.sessionState = 'live'; return reply(ws, req, 'design_done', null);
      case 'design_finish_confirm':
        // As archond: approving with no note in force needs the operator's yes.
        if (state.unconfirmedNoChange && !msg.confirm_no_change) return refuse(ws, req, 'invalid', 'invalid argument: nothing changed in this session; confirm that you want to approve the design as it is');
        state.confirmed = msg; state.sessionPatch = { ...state.sessionPatch }; state.sessionState = 'confirmed'; state.version += 1;
        broadcast({ event: 'session', state: 'confirmed', degraded: false, preview_path: '/', commit: null, browser: { status: 'running' }, note_version: state.version });
        return reply(ws, req, 'design_done', null);
      default: return reply(ws, req, 'design_done', null);
    }
  }
  return {
    state,
    connect(ws) {
      const conn = { ws, subscribed: false };
      state.connections.add(conn);
      ws.onMessage((raw) => handle(conn, JSON.parse(String(raw))).catch((e) => console.error('stand-in daemon', e)));
      ws.onClose(() => state.connections.delete(conn));
    },
    // A session event as archond pushes it, at the current note version.
    pushSession(fields) {
      if ('degraded' in fields) state.sessionPatch.degraded = fields.degraded;
      broadcast({ event: 'session', state: state.sessionState, degraded: false, preview_path: '/', commit: null, browser: { status: 'running' }, note_version: state.version, ...fields });
    },
    // The app stopped answering: no frames until resumed.
    // Fields of the session as design_get reports it (degraded, preview_url, ...).
    patchSession(patch) { Object.assign(state.sessionPatch, patch); },
    pauseFrames() { state.paused = true; },
    resumeFrames() { state.paused = false; state.connections.forEach((c) => sendFrame(c)); },
    // The network dropped every open socket; the web app reconnects on its own.
    dropSockets() { state.connections.forEach((c) => c.ws.close()); state.connections.clear(); },
    // A new session replaced this one: every request now answers `gone`.
    endSession(reason = 'revised') { state.ended = true; state.sessionState = 'ended'; state.endReason = reason; broadcast({ event: 'ended', reason }); },
    // The agent reran after a revision: design_get now answers with a new live session.
    startNewSession(id = 'ds_SECOND') { state.ended = false; state.sessionId = id; state.sessionState = 'live'; state.endReason = null; state.seq = 0; },
    stop() { state.ended = true; },
  };
}
