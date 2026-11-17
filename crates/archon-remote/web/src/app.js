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
import { APP_VERSION } from './version.js';

const REFRESH_DEBOUNCE_MS = 350;
const SESSION_CHECK_MS = 10_000;
const RECENT_LIMIT = 200;
const FEED_RETRY_MS = [2_000, 5_000, 15_000, 30_000];

export function createApp({ session, onUnpaired }) {
  if (!session?.device) throw new TypeError('the app starts with a paired session');
  const store = createStore({
    session, connection: { state: 'connecting' }, summaries: null, questions: null, status: null,
    cost: null, repos: null, reports: null, feed: [], conversation: [], outbox: [], chatNotes: {}, errors: {},
  });
  let transport = null;
  const request = (cmd, fields, options) => transport.request(cmd, fields, options);
  const cmds = daemonCommands(request);
  const feed = createFeed({ request });
  const changes = createChanges({ request });
  const refresh = createLoaders(store, cmds);
  const scheduleRefresh = coalesce(() => refresh.many(['summaries', 'questions', 'status']), REFRESH_DEBOUNCE_MS);
  const checkSession = sessionChecker(() => stop(), onUnpaired);

  feed.subscribe((fresh, items) => {
    store.set({ feed: items });
    if (fresh.some((item) => !item.replayed && drivesRefresh(item))) scheduleRefresh();
    if (fresh.some((item) => item.entry?.entry === 'report')) refresh.one('reports');
  });
  changes.subscribe((change) => {
    if (change.subject === 'pause') applyPause(store, change);
    scheduleRefresh();
  });

  const isOpen = () => transport.state === 'open';
  let welcome = null;
  const feedSync = streamSyncer({ name: 'feed', sync: () => feed.sync(), store, isOpen });
  const changesSync = streamSyncer({ name: 'changes', sync: () => changes.sync(welcome), store, isOpen });
  const conversation = createConversation({
    request, isOpen,
    onChange: (entries, fresh) => {
      store.set({ conversation: entries });
      matchEchoes(store, fresh);
    },
    onError: (err) => store.set({ errors: { ...store.get().errors, conversation: err } }),
  });
  transport = openTransport({ session, feed, changes, store, onReady: (welcomed) => {
    welcome = welcomed;
    feedSync.start();
    changesSync.start();
    conversation.run();
    refresh.many(['summaries', 'questions', 'status', 'repos', 'reports', 'cost']);
  } });
  store.watch((s) => s.connection.state, (state) => {
    if (state === 'revoked') {
      stop();
      onUnpaired('This device was unpaired on the host.');
    } else if (state === 'waiting') {
      checkSession();
    }
  });

  const onVisible = () => {
    if (document.visibilityState !== 'visible') return;
    transport.reconnectNow();
    if (transport.state === 'open') refresh.many(['summaries', 'questions', 'status']);
  };

  function stop() {
    document.removeEventListener('visibilitychange', onVisible);
    if (transport.state !== 'stopped') transport.stop();
  }

  return {
    store, cmds, feed, changes, refresh,
    // Fixed at pairing: an observer's app shows no control that changes Archon.
    canOperate: canOperate(session.device),
    resyncFeed: () => feedSync.run(),
    resyncConversation: () => conversation.run(),
    start() {
      document.addEventListener('visibilitychange', onVisible);
      transport.start();
    },
    stop,
    get transport() { return transport; },
  };
}

function openTransport({ session, feed, changes, store, onReady }) {
  const setError = (name, err) => store.set({ errors: { ...store.get().errors, [name]: err } });
  return createTransport({
    url: `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${BASE}/ws`,
    hello: { protocol_version: session.protocol_version, client: 'archon-web', client_version: APP_VERSION },
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
      const delay = FEED_RETRY_MS[Math.min(failures, FEED_RETRY_MS.length - 1)];
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

// A connection that keeps failing may mean the device was revoked while
// the socket was down; the session endpoint says so without a socket.
function sessionChecker(stop, onUnpaired) {
  let lastCheck = 0;
  return async () => {
    if (Date.now() - lastCheck < SESSION_CHECK_MS) return;
    lastCheck = Date.now();
    // Unreachable is expected while offline; only a definite 401 unpairs.
    const current = await api.session().catch(() => undefined);
    if (current === null) {
      stop();
      onUnpaired('This device is no longer paired.');
    }
  };
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

// Runs `fn` once, `ms` after the first of a burst of calls. Unlike a debounce,
// a steady stream of events cannot postpone it forever.
function coalesce(fn, ms) {
  let timer = null;
  return () => {
    if (timer !== null) return;
    timer = setTimeout(() => {
      timer = null;
      fn();
    }, ms);
  };
}

// A pause change carries the whole new pause state; the status reload follows.
function applyPause(store, change) {
  if (typeof change.global !== 'boolean' || !Array.isArray(change.repos) || !Array.isArray(change.pipelines)) {
    throw new TypeError(`a pause change carries the pause state: ${JSON.stringify(change)}`);
  }
  const status = store.get().status;
  if (!status) return;
  store.set({ status: { ...status, pause: { global: change.global, repos: change.repos, pipelines: change.pipelines } } });
}
