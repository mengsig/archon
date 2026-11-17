// The paired app: one transport, the live feed, the store every screen
// reads, and the loaders that keep it fresh.
import { createTransport } from './core/transport.js';
import { createFeed } from './core/feed.js';
import { createChanges } from './core/changes.js';
import { createConversation } from './core/conversation.js';
import { createStore } from './core/store.js';
import { daemonCommands } from './core/daemon.js';
import { ACTIVE_STATES, stateKind, isTerminal, waitsOnOperator } from './core/format.js';
import * as api from './core/api.js';
import { BASE } from './core/base.js';
import { matchEchoes } from './features/chat/outbox.js';
import { canOperate } from './core/role.js';
import { coalesce } from './core/live-query.js';
import { APP_VERSION } from './version.js';
import { deviceGuard, sessionChecker } from './core/session-watch.js';
import { sessionSignal } from './core/session-signal.js';
import { retryDelay, STREAM_BACKOFF } from './core/backoff.js';

const REFRESH_DEBOUNCE_MS = 350;
const SESSION_CHECK_MS = 10_000;
// An unanswered session read after a pairing signal retries at most this far apart.
const SESSION_RETRY = Object.freeze({ baseMs: 500, capMs: SESSION_CHECK_MS });
const RECENT_LIMIT = 200;

export function createApp({ session, onUnpaired, onDeviceChanged }) {
  if (!session?.device) throw new TypeError('the app starts with a paired session');
  if (typeof onDeviceChanged !== 'function') throw new TypeError('the app reports a changed device');
  const store = createStore({
    session, connection: { state: 'connecting' }, summaries: null, questions: null, status: null,
    cost: null, repos: null, reports: null, feed: [], conversation: [], outbox: [], chatNotes: {}, errors: {}, sessionUnsure: false,
  });
  let transport = null;
  const request = (cmd, fields, options) => transport.request(cmd, fields, options);
  const cmds = daemonCommands(request);
  const feed = createFeed({ request });
  const changes = createChanges({ request });
  const refresh = createLoaders(store, cmds);
  const scheduleRefresh = coalesce(() => refresh.many(['summaries', 'questions', 'status']), REFRESH_DEBOUNCE_MS);
  canOperate(session.device);
  const guard = deviceGuard(session.device, () => {
    stop();
    onDeviceChanged();
  });
  const sessionCheck = sessionChecker({
    readSession: api.session, guard, stop: () => stop(), onUnpaired, minGapMs: SESSION_CHECK_MS,
    onUnsure: (unsure) => store.set({ sessionUnsure: unsure }), limits: SESSION_RETRY,
  });

  followStreams({ store, feed, changes, refresh, scheduleRefresh });

  const isOpen = () => transport.state === 'open';
  let welcome = null;
  const feedSync = streamSyncer({ name: 'feed', sync: () => feed.sync(), store, isOpen });
  const changesSync = streamSyncer({ name: 'changes', sync: () => changes.sync(welcome), store, isOpen });
  const conversation = openConversation({ request, isOpen, store });
  transport = openTransport({ session, feed, changes, store, onReady: (welcomed) => {
    if (welcomed?.reply !== 'welcome' || !welcomed.data) throw new TypeError(`hello answered ${welcomed?.reply}`);
    if (welcomed.data.device) guard(welcomed.data.device);
    welcome = welcomed.data;
    feedSync.start();
    changesSync.start();
    conversation.run();
    refresh.many(['summaries', 'questions', 'status', 'repos', 'reports', 'cost']);
  } });
  watchConnection({ store, stop: () => stop(), checkSession: () => sessionCheck.check(), onUnpaired });

  // A connected tab is re-paired or forgotten in another tab without its socket noticing.
  const recheckSession = () => sessionCheck.check({ force: true });
  let unlisten = null;
  const onVisible = () => {
    if (document.visibilityState !== 'visible') return;
    recheckSession();
    transport.reconnectSoon();
    // One read now, which also takes the place of any burst refresh still waiting.
    if (transport.state === 'open') scheduleRefresh.flush();
  };

  function stop() {
    sessionCheck.cancel();
    unlisten?.();
    unlisten = null;
    document.removeEventListener('visibilitychange', onVisible);
    if (transport.state !== 'stopped') transport.stop();
  }

  return {
    store, cmds, feed, changes, refresh,
    // An app is built for one device and role; a change of either remounts it.
    canOperate: canOperate(session.device),
    resyncFeed: () => feedSync.run(),
    resyncConversation: () => conversation.run(),
    start() {
      if (unlisten) throw new Error('the app already started');
      unlisten = sessionSignal().listen(recheckSession);
      document.addEventListener('visibilitychange', onVisible);
      transport.start();
    },
    stop,
    get transport() { return transport; },
  };
}

// Feed entries and committed changes patch the store or reload what they touch.
function followStreams({ store, feed, changes, refresh, scheduleRefresh }) {
  if (typeof scheduleRefresh !== 'function' || typeof refresh?.one !== 'function') throw new TypeError('the streams need the loaders');
  feed.subscribe((fresh, items) => {
    store.set({ feed: items });
    if (fresh.some((item) => !item.replayed && drivesRefresh(item))) scheduleRefresh();
    if (fresh.some((item) => item.entry?.entry === 'report')) refresh.one('reports');
  });
  changes.subscribe(() => scheduleRefresh());
}

function watchConnection({ store, stop, checkSession, onUnpaired }) {
  if (typeof stop !== 'function' || typeof checkSession !== 'function') throw new TypeError('the connection watch needs stop and the session check');
  if (typeof onUnpaired !== 'function') throw new TypeError('the connection watch reports an unpairing');
  store.watch((s) => s.connection.state, (state) => {
    if (state === 'revoked') {
      stop();
      onUnpaired('This device was unpaired on the host.');
    } else if (state === 'waiting') {
      checkSession();
    }
  });
}

// The conversation's entries land in the store, matched against the outbox.
function openConversation({ request, isOpen, store }) {
  if (typeof request !== 'function' || typeof isOpen !== 'function') throw new TypeError('the conversation needs requests and the socket state');
  return createConversation({
    request, isOpen,
    onChange: (entries, fresh) => {
      store.set({ conversation: entries });
      matchEchoes(store, fresh);
    },
    onError: (err) => store.set({ errors: { ...store.get().errors, conversation: err } }),
  });
}

function openTransport({ session, feed, changes, store, onReady }) {
  const setError = (name, err) => store.set({ errors: { ...store.get().errors, [name]: err } });
  return createTransport({
    url: `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${BASE}/ws`,
    hello: { protocol_version: session.protocol_version, client: 'archon-web', client_version: APP_VERSION,
      features: ['onto_edges'] },
    onFeed: (item) => feed.push(item),
    onChange: (change) => changes.push(change),
    onReady,
    onState: (state, info) => store.set({ connection: { state, ...info } }),
    onProtocolError: (err) => setError('protocol', err),
  });
}

// A stream's setup that keeps trying while the socket is open: a failed history
// or subscription would otherwise leave the app connected but no longer live.
function streamSyncer({ name, sync, store, isOpen }) {
  if (typeof sync !== 'function' || typeof isOpen !== 'function') throw new TypeError('streamSyncer needs the sync and the socket state');
  if (typeof name !== 'string' || !name) throw new TypeError('a stream names its error slot');
  const setError = (err) => store.set({ errors: { ...store.get().errors, [name]: err } });
  let timer = null;
  let running = false;
  let failures = 0;
  const run = () => {
    if (running || !isOpen()) return;
    clearTimeout(timer);
    timer = null;
    running = true;
    sync().then(() => {
      failures = 0;
      if (store.get().errors[name]) setError(null);
    }, (err) => {
      setError(err);
      const delay = retryDelay(failures, { limits: STREAM_BACKOFF });
      failures += 1;
      if (isOpen()) timer = setTimeout(run, delay);
    }).finally(() => { running = false; });
  };
  return {
    run,
    start() {
      failures = 0;
      run();
    },
  };
}

function drivesRefresh(item) {
  const kind = item.entry?.entry;
  return kind !== 'operator' && kind !== 'orchestrator';
}

function createLoaders(store, cmds) {
  const sources = {
    summaries: () => loadSummaries(cmds),
    questions: () => cmds.questions(),
    status: () => cmds.status(),
    cost: () => cmds.costReport('repo'),
    repos: () => cmds.repos(),
    reports: () => cmds.reports(),
  };
  const inflight = new Map();
  // Resolves true when fresh data landed; a failure is kept in errors[name].
  function one(name) {
    const source = sources[name];
    if (!source) throw new RangeError(`no loader named ${name}`);
    if (inflight.has(name)) return inflight.get(name);
    const run = source().then(
      (data) => { store.set({ [name]: data, errors: { ...store.get().errors, [name]: null } }); return true; },
      (err) => { store.set({ errors: { ...store.get().errors, [name]: err } }); return false; },
    ).finally(() => inflight.delete(name));
    inflight.set(name, run);
    return run;
  }
  return { one, many: (names) => Promise.all(names.map(one)) };
}

// Every active item, plus the newest of all, so old active work is never cut off.
async function loadSummaries(cmds) {
  const [active, recent] = await Promise.all([
    cmds.summaries({ states: ACTIVE_STATES }),
    cmds.summaries({ limit: RECENT_LIMIT }),
  ]);
  const byId = new Map();
  for (const item of [...recent, ...active]) byId.set(item.id, item);
  return [...byId.values()].sort(compareSummaries);
}

const PRIORITY_RANK = { critical: 0, high: 1, normal: 2, low: 3 };

function rank(summary) {
  if (waitsOnOperator(summary)) return 0;
  const kind = stateKind(summary);
  if (kind === 'running' || kind === 'dispatching') return 1;
  return isTerminal(summary) ? 3 : 2;
}

function compareSummaries(a, b) {
  const byRank = rank(a) - rank(b);
  if (byRank !== 0) return byRank;
  if (rank(a) < 3) {
    const byPriority = (PRIORITY_RANK[a.priority] ?? 2) - (PRIORITY_RANK[b.priority] ?? 2);
    if (byPriority !== 0) return byPriority;
  }
  return Date.parse(b.updated_at) - Date.parse(a.updated_at);
}
