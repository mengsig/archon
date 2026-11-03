# vNext shared contracts (issue #377, phase 0)

These are the interfaces that the workstreams in [WORKSTREAMS.md](WORKSTREAMS.md)
build against. Each is marked:

- **Frozen**: fixed for phase 1. A change needs an edit to this file on the
  integration branch, approved in review, before any code depends on it.
  Adding an *optional* field is not a change.
- **Open**: the direction is set but the owner settles the shape. The owner
  records it here in the same PR that implements it.

Names use the wire spelling (snake_case JSON). `?` marks an optional field.
`Ts` is an RFC 3339 UTC timestamp. Ids keep their existing prefixes (`qi_`,
`run_`, `agt_`, `mem_`, …).

## 1. Daemon protocols

### 1.1 Aether protocol 26: unified-frontend read models (owner AE-P, Frozen)

`PROTOCOL_VERSION` becomes 26. `MIN_PROTOCOL_VERSION` stays 17. Every
addition is listed in `Command::introduced()` and advertised in
`Welcome.features`.

| Item | Shape | Class | Feature |
| --- | --- | --- | --- |
| `list_agents_page` | `{query: AgentQuery, after?: AgentId, limit?: u32 (1..=500, default 200)}` → reply `agents_page {agents: Agent[], next_after?: AgentId}`. Ordered by `AgentId` ascending. | observe | `agents_paging` |
| `get_tree` (extended) | adds optional `role_prefix?: string`, `root?: AgentId`. Absent fields keep today's behaviour. | observe | `agents_paging` |
| `get_transcript_page` | `{agent, after?: u64, before?: u64, limit?: u32 (1..=1000, default 200)}` → reply `transcript_page {entries: TranscriptEntry[], has_before: bool, has_after: bool, last_seq: u64}`. `after` and `before` are mutually exclusive. | observe | `transcript_paging` |
| Delta ordering | every `delta` frame gains `invocation_seq: u64`. The runtime assigns it once, when the delta is produced and before fan-out or filtering. It starts at 1 for each `(agent, invocation)`, increases by 1 per delta, and is the same on every subscription. A delta lost on the bus or by a lagging subscriber therefore shows as a gap, and resubscribing never renumbers. `tool_input` gains `tool_use_id?: string`. Sent only to clients that list `delta_order`. | n/a | `delta_order` |
| `usage_breakdown` | `{scope: {run?: RunId, agent?: AgentId, subtree?: AgentId, role_prefix?: string}, by: "model"\|"provider"\|"role"\|"day"\|"run", since?: Ts, until?: Ts, limit?: u32 (1..=1000)}` → reply `usage_breakdown {rows: [{key: string, usage: Usage, invocations: u64}], total: Usage, truncated: bool}`. Deterministic: rows ordered by `key`. It keeps `Usage.cost_complete/cost_gap/cost_basis/estimated_cost_usd`. | observe | `usage_breakdown` |
| Remote mounts | `RemoteSettings.mounts: [{prefix: string, kind: "archon", target: string}]` (at most 4; `prefix` matches `^/[a-z][a-z0-9-]{0,31}$`; `target` is `http://127.0.0.1:<port>` or `http://[::1]:<port>`). Read by `get_remote`, written by `configure_remote` (both remote_admin). `GET /api/health` gains `mounts: [{prefix, kind}]`. | remote_admin | `remote_mounts` |

### 1.2 Aether protocol 26: later additions (same version, own feature)

| Item | Owner | Status |
| --- | --- | --- |
| Noesis commands and events, §6 | F3 | Frozen names and required fields; optional fields open |
| Memory 2.0 commands and fields, §7 | G3 | Open |

### 1.3 Archon protocol 24 (owner AR-P for the wire, Frozen)

AR-P adds every wire type below with its `Request::class()` entry and a
handler that returns `unavailable` ("not implemented yet"). The owner named in
each row replaces the stub. The owners' tasks never edit `protocol.rs`
dispatch or class tables, except to add optional fields.

**Negotiation and standing**

- `hello {protocol_version, client, client_version, features?: string[]}`. For
  `client = "archon-web"` the daemon accepts 23..=24. Other clients stay
  exact-match (one binary).
- `Welcome {protocol_version, daemon_version, pid, min_protocol_version: u32, features: string[], device?: {id, name, role: "operator"|"observer"}}`.
  `protocol_version` is the version the connection speaks (the client's,
  once accepted) and `min_protocol_version` the oldest archond accepts from
  that client kind. A client at 23 is sent no features.
- Feature strings: `changes`, `task_graph`, `run_plan`, `item_history`,
  `item_filters`, `github_links`, `github_import`, `device_roles`.
- Requests from the remote listener run with `ClientKind::ArchonWeb` standing,
  not `ArchonCli` (enforced in AR-R).
- New `ErrorCode::history_expired {first_seq, last_seq}`, on the wire
  `"code": {"history_expired": {"first_seq": …, "last_seq": …}}`; the other
  codes stay plain strings.
- `archon schema` prints the schema, and `archon schema --out
  crates/archon-core/protocol.schema.json` writes the committed copy
  (schemars, JSON Schema 2020-12) covering `ClientFrame` and `ServerFrame`.
  A test fails when the file is stale.

**Changes and history (owner AR-C)**

| Item | Shape | Class |
| --- | --- | --- |
| `subscribe_changes` | `{after?: u64}` → `subscribed {id, last_seq}`, then frames `{"frame":"change","sub":id,"change":Change}` | Read |
| `changes_page` | `{after: u64, limit?: u32 (1..=1000)}` → `changes {changes: Change[], last_seq}` | Read |
| `Change` | `{seq: u64, at: Ts, subject: ChangeSubject}`. `seq` is gapless and written in the same transaction as the state it describes. The subject is flattened: `{"seq":7,"at":…,"subject":"item","item":"qi_…","state":"running"}`. | |
| `ChangeSubject` (tag `subject`) | `item {item, state: QueueStateKind, stage?: string, waiting_reason?: WaitingReasonCode}` · `dependency {item, depends_on, added: bool}` · `run {run, item, state: RunStateKind}` · `stage {stage_run, run, item, stage_id, attempt, fan_index, state: StageStateKind, verdict?: Verdict}` · `gate {run, item, stage_id, decision?: GateDecision}` · `question {question, item?, open: bool}` · `initiative {initiative, state: InitiativeState}` · `link {item, host, repo, number, relation}` | |
| `list_item_summaries` (extended) | with `with_as_of: true` in its query the reply is `item_summaries_at {summaries: ItemSummary[], as_of: u64}`, `as_of` read in the same database read transaction as the summaries. Without it the reply stays the bare `item_summaries` array protocol 23 reads. | Read |
| `item_history` | `{item, limit?: u32 (1..=1000)}` → `item_history {item, transitions: [{seq, at, from?: QueueStateKind, to: QueueStateKind, reason?: string, by: "daemon"\|"cli"\|"web"\|"mcp"\|"github"}]}` | Read |

Retention: changes are kept for at least 30 days or the newest 100,000 rows,
whichever keeps more. Older cursors get `history_expired`.

**Graph, plans and filters (owner AR-G)**

| Item | Shape | Class |
| --- | --- | --- |
| `task_graph` | `{filter: {repos?: RepoId[], states?: QueueStateKind[], pipelines?: string[], min_priority?: Priority, initiative?: InitiativeId, component_of?: QueueItemId, include_terminal_since?: Ts}, limit?: u32 (1..=2000, default 500)}` → `task_graph {nodes: TaskNode[], edges: TaskEdge[], initiatives: [{id, title, state, atomic}], as_of: u64, truncated: bool}` | Read |
| `TaskNode` | `{id, repo?, title, pipeline, priority, state: QueueStateKind, state_detail?: string, stage?: string, waiting_reason?: WaitingReasonCode, spent_usd: f64, initiative?, tags: string[], created_at, updated_at, issue?: GithubRef, pr?: PrRef, agents_running: u32}` | |
| `TaskEdge` | `{from: QueueItemId (prerequisite), to: QueueItemId (dependent), kind: "depends_on"\|"initiative_after"\|"stacked_on", satisfied: bool}` | |
| `run_plan` | `{run}` → `run_plan {run, pipeline, revision: string, stages: StageSummary[], edited: bool}`: the frozen pipeline with steer edits applied | Read |
| `list_items` (extended) | adds `updated_since?: Ts`, `before?: Ts`, `text?: string (1..=200 chars, title and brief substring)`, also on `list_item_summaries`. Until `item_filters` is served, a query using them is refused with `unavailable`. | Read |

`as_of` is the last change seq committed when the projection was read, taken in
the **same read transaction** as the rows, for `task_graph` and
`list_item_summaries` alike. Nodes are ordered by
`(created_at, id)` and edges by `(from, to, kind)`, so the same store always
gives the same bytes.

**GitHub links (owners D1, D2, D3)**

| Item | Shape | Class | Owner |
| --- | --- | --- | --- |
| `GithubRef` | `{host, repo: "owner/name", number: u64, kind: "issue"\|"pull_request"}` | | D1 |
| `PrRef` | `GithubRef` plus `state?: "open"\|"draft"\|"merged"\|"closed"`, `review_decision?`, `checks?: {state: "pending"\|"success"\|"failure"\|"neutral", total: u32, failed: u32}` | | D1 / D3 |
| `GithubLink` | `{item, ref: GithubRef, relation: "origin"\|"imported"\|"delivered"\|"closes"\|"mentioned", source: "watcher"\|"operator"\|"import"\|"ship"\|"backfill", created_at}` | | D1 |
| `item_links` | `{item}` → `item_links {links: GithubLink[], objects: GithubObject[]}` | Read | D1 |
| `issue_items` | `{ref: GithubRef}` → `issue_items {links: GithubLink[]}` | Read | D1 |
| `link_issue` / `unlink_issue` | `{item, ref, relation: "closes"\|"mentioned"}` → `ok`. Idempotent: the same link twice is `ok`. | Change | D1 |
| `GithubObject` | `{ref, title, state: "open"\|"closed"\|"merged", state_reason?, draft?, labels: string[], author?, updated_at, fetched_at, review_decision?, checks?}` | | D2 |
| `list_github_objects` | `{repo: "owner/name", kind?, state?, limit?: u32 (≤500)}` → `github_objects {objects, synced_at?}` | Read | D2 |
| `import_issue` | `{ref, pipeline?, priority?}` → `item`. Its idempotency key is derived as `github:<host>/<owner>/<repo>#<number>`; a repeat returns the first item. | Change | D2 |
| `ItemSummary` (extended) | adds `issue?: GithubRef`, `pr?: PrRef` (keeps `pr_url`) | | D1 / D3 |

### 1.4 Store migrations (pre-assigned, Frozen)

| Store | Version | Owner | Content |
| --- | --- | --- | --- |
| Archon `archon.db` | V19 | AR-C | `changes`, transitions |
| | V20 | D1 | `github_links` plus backfill |
| | V21 | D2 | `github_objects` |
| | V22 | D3 | `github_outbox` |
| | V23 | G5 | lessons 2.0 columns |
| Aether `aether.db` | V29 | AE-P | `agents.subsystem` (nullable text, set once at creation; `"noesis"` for Noesis agents), plus indexes for paging and `usage_breakdown` |
| | V30 | G1 | memory tiers, candidate state, support, provenance refs |
| | V31 | G2 | retrieval log and holdout |
| | V32 | G3 | migration bookkeeping, if needed |
| Noesis `noesis.db` | N1 | F1 | sessions, messages index, questions, plans, graph ledgers, coverage, jobs |
| | N2 | F4 | learner tables (§6.4) |
| | N3 | F6 | evidence bundles |

Each task appends exactly its own line to its store's migration list. An
unused slot ships as a no-op migration, so the numbering stays fixed.

### 1.5 Version rules (Frozen)

- Each daemon's protocol version changes once per release, in the first
  protocol task (AE-P: 26, AR-P: 24).
- Within an unreleased version, additions are optional fields, or new commands
  and frames behind a new feature string. Nothing shipped in an earlier
  release changes shape.
- Every new command gets an access class (Aether `access.rs`, Archon
  `Request::class()`), a remote-allowlist decision (AR-R / `authz`), a schema
  entry, and regenerated clients (`scripts/generate_clients.py` / `archon schema`).
- Product versions (Aether 1.3.0, Archon 1.5.0) change only in H5.

## 2. Frontend modules (Frozen layout; owners as listed)

### 2.1 Package layout (Aether repository, `web/`)

| Path | Package | Owner | Public API |
| --- | --- | --- | --- |
| `web/packages/protocol/` | `@aether-ui/protocol` | A1 | generated `aether.ts` and `archon.ts` types and command builders, plus the hand-written `app.ts` (§2.5). No runtime code beyond type guards. |
| `web/packages/core/` | `@aether-ui/core` | A2 | `createApp(host: HostAdapter)`, `ConnectionManager`, `Transport` (`ws-aether`, `ws-archon`, `ipc`), `Store`, `useSelector`, `useCommand`, `useCapabilities`, `registerRoutes`, `registerEntityAction`, the router (§2.3, §2.5) |
| `web/packages/ui/` | `@aether-ui/ui` | A3 | `tokens.css`, `ThemeProvider`, primitives (Button, Field, Menu, Dialog, Sheet, Tabs, Toast, Tooltip, Badge, StatusPill, Skeleton, EmptyState, ErrorState), `AppShell` (routes and actions as props), `NavRail`, `CommandPalette`, `VirtualList` |
| `web/packages/graph/` | `@aether-ui/graph` | A4 | `GraphCanvas` (Canvas 2D, dirty-flag rAF), `Viewport` (pan, zoom, fit), `layeredLayout` (worker; deterministic; cached by structure hash), `forceLayout`, `GraphModel` (nodes, edges, groups, collapse state) |
| `web/packages/aether/` | `@aether-ui/aether` | B1 `src/fleet`, `src/topology`; B2 `src/agent`, `src/conversation`; B3 `src/controls`, `src/inbox`, `src/activity`, `src/memory` (extended later by G3) | each directory exports `routes.ts` |
| `web/packages/archon/` | `@aether-ui/archon` | C1 `src/queue`, `src/item`, `src/pipeline`; C2 `src/dag`, `src/initiatives`, `src/history` | `routes.ts` per directory |
| `web/packages/github/` | `@aether-ui/github` | D4 | `routes.ts`, entity actions |
| `web/packages/noesis/` | `@aether-ui/noesis` | F5 (F6 `src/understand`) | `routes.ts`, entity actions |
| `web/packages/settings/` | `@aether-ui/settings` | B3 | `routes.ts` |
| `web/apps/web/` | app | A3 | browser entry, `sw.js`, manifest |
| `web/apps/desktop/` | app | E1 `src/{main,preload,bridge,security}`; E2 `src/{native,discovery,lifecycle}`; E3 packaging files | §2.4 |
| `web/e2e/` | tests | H1 harness and `specs/smoke`; H4 `specs/flows`; F7 `specs/noesis` | |
| `web/dist/`, `web/schemas/` | generated | SYNC | |

A3 creates the empty skeleton (`package.json`, `src/index.ts`) for every
feature package. Each `index.ts` loads its directories' `routes.ts` with
`import.meta.glob`, so feature tasks never edit a shared index.

### 2.2 Import rules (Frozen, enforced by lint in A1)

- `protocol` imports nothing. `graph` and `ui` import only React and
  `protocol` types. `core` imports `protocol`.
- Feature packages import `core`, `ui`, `graph` and `protocol`, **never each
  other**. They cross over only through routes (§2.3) and the entity-action
  registry.
- Only `core` talks to transports. Feature code sends through `useCommand`,
  which adds idempotency keys, role checks and error reporting.
- No feature keeps its own copy of daemon entities (agents, items, stages,
  links). Features read through selectors. Component-local state is for view
  state only (selection, scroll, expansion).

### 2.3 Routes (Frozen)

The router is hash-based, because Aether's listener mounts `/archon/*` and
Electron serves `app://`.

| Route | Owner |
| --- | --- |
| `#/fleet`, `#/fleet/topology` | B1 |
| `#/agents/:conn/:agent` | B2 |
| `#/inbox`, `#/activity`, `#/controls/:conn/:agent`, `#/memory/:conn` | B3 |
| `#/archon/queue`, `#/archon/items/:item`, `#/archon/items/:item/runs/:run` | C1 |
| `#/archon/graph`, `#/archon/initiatives/:id`, `#/archon/history` | C2 |
| `#/github/:host/:owner/:repo/:number` | D4 |
| `#/learn`, `#/learn/sessions/:id`, `#/learn/graph`, `#/learn/me` | F5 |
| `#/settings/*` | B3 |
| `#/pair/:daemon` | A2 (flow) / A3 (screen) |

`:conn` is the connection id (§5.1). Entity kinds for actions: `agent`, `run`,
`item`, `stage`, `initiative`, `issue`, `pull_request`, `proposition`.

### 2.4 Desktop bridge `window.desktop` (Frozen, E1 implements, A2 consumes)

```ts
interface DesktopBridge {
  readonly version: 1;
  discover(): Promise<LocalDaemon[]>;            // {id, kind: "aether"|"archon", label, reachable, version?}
  connect(daemon: string): Promise<string>;      // returns a connection id
  request(conn: string, frame: unknown): Promise<unknown>;   // validated ClientFrame → reply frame
  subscribe(conn: string, frame: unknown, onFrame: (f: unknown) => void): Promise<() => void>;
  close(conn: string): Promise<void>;
  privileged(action: PrivilegedAction): Promise<unknown>;   // main builds frame and confirmation text
  startDaemon(kind: "aether" | "archon"): Promise<void>;     // only if desktop.managed_start
  pickDirectory(purpose: string): Promise<string | null>;
  notify(n: {title: string; body: string; route?: string}): void;
  openExternal(url: string): Promise<void>;      // https: only
  openRemote(origin: string): Promise<void>;     // an entry of remote.origins; opens a remote window (ARCHITECTURE.md §c)
  onDeepLink(cb: (route: string) => void): () => void;       // aether-app://open?route=…
  diagnostics(): Promise<{versions: Record<string, string>; daemons: LocalDaemon[]; logPath: string}>;
  getPreference(key: PreferenceKey): Promise<unknown>;
  setPreference(key: PreferenceKey, value: unknown): Promise<void>;
}
type PreferenceKey = "window.bounds" | "layout.panels" | "theme" | "desktop.managed_start" | "remote.origins";

// Closed set. A new kind is a CONTRACTS.md edit. `conn` must be a local connection.
type PrivilegedAction =
  | {kind: "memory.accept" | "memory.reject"; conn: string; memory: string}
  | {kind: "device.pair"; conn: string; name: string; role: "operator" | "observer"}
  | {kind: "device.revoke"; conn: string; device: string}
  | {kind: "remote.enable" | "remote.disable"; conn: string}
  | {kind: "remote.mount"; conn: string; prefix: "/archon"; target_port: number}
  | {kind: "noesis.configure"; conn: string; patch: NoesisSettingsPatch}   // host-only keys of §6.1
  | {kind: "archon.operator_change"; conn: string; request: "merge" | "unblock" | "settings"; item?: string; patch?: unknown}
  | {kind: "engine.login"; conn: string; engine: "claude" | "codex"};      // opens a terminal; no secret crosses IPC
```

Main validates every argument and size: frames at most 1 MiB, strings at most
4 KiB. `request` and `subscribe` refuse any command outside the pass-through
classes (ARCHITECTURE.md §c), and refuse `shutdown` whatever its class.
`privileged` accepts only the `PrivilegedAction` union, and only for a
connection that main discovered locally. Main:
- builds the daemon frame itself from the validated fields;
- writes the native confirmation text itself, from main-owned templates
  holding the daemon label and socket path. Renderer-supplied values appear
  only as validated argument values in fixed slots, with control characters
  escaped:
  - *Descriptive labels* (device name, memory first line) may be cut to 200
    characters, with an ellipsis shown.
  - *Consequential values* are always shown in full and never cut: role,
    memory id, mount prefix and port, `request` kind, item id, and **every
    changed key of a settings patch with its old and new value**.
  - Patches are limited to known keys (§6.1, Archon settings), so their full
    rendering is bounded. If the full rendering exceeds 60 lines, main
    refuses the action and asks for smaller patches; it never shows a
    partial diff;
- sends exactly the confirmed frame.
The renderer supplies no frame and no display text. Calls from any frame
other than the top-level `app://` document are refused.

`remote.origins` holds at most 16 exact `https://host[:port]` origins: no
path, query, fragment or userinfo, normalized to lowercase ASCII (IDN as
punycode). `openRemote` refuses any other value. A remote window is
unprivileged (ARCHITECTURE.md §c): it has no preload and no bridge, its
title shows the origin, and main holds no IPC channel to it. Remote
connections therefore never use the `ipc` transport; inside a remote window
`core` runs as `host.browser` with `ws-aether` and `ws-archon`.

### 2.5 Shared frontend types (Frozen; A1 writes them, A2/A3/A4 implement)

These types live in `web/packages/protocol/src/app.ts`, so `ui` and `graph`
can use them without importing `core`. Runtime registries are in `core`.

```ts
type ConnId = string;                                   // §5.1
type Standing = "local" | "operator" | "observer";
type ConnState = "connecting" | "pairing" | "ready" | "backoff" | "closed";
interface ConnectionInfo { id: ConnId; daemon: "aether" | "archon"; state: ConnState;
  protocol?: number; features: readonly string[]; standing?: Standing; label: string }
type Capabilities = Readonly<Record<string, boolean | string>>;   // keys of §3

interface HostAdapter { readonly kind: "browser" | "electron";
  readonly desktop?: DesktopBridge; readonly storage: KeyValueStore }
interface KeyValueStore { get(key: string): Promise<unknown>; set(key: string, v: unknown): Promise<void> }
interface Transport { readonly id: "ws-aether" | "ws-archon" | "ipc";
  open(target: string, onFrame: (f: unknown) => void, onClose: (code: number, reason: string) => void):
    Promise<{ send(frame: unknown): void; close(): void }> }

type EntityKind = "agent" | "run" | "item" | "stage" | "initiative" | "issue" | "pull_request" | "proposition";
interface EntityRef { kind: EntityKind; conn: ConnId; id: string }
interface RouteDef { path: string; owner: string;               // path as in §2.3
  nav?: { label: string; section: "aether" | "archon" | "github" | "learn" | "settings" };
  requires?: (caps: Capabilities) => boolean;
  load: () => Promise<{ default: unknown /* React component of RouteProps */ }> }
interface RouteProps { params: Readonly<Record<string, string>> }
interface EntityAction { id: string; kind: EntityKind; label: string;
  requires?: (caps: Capabilities, ref: EntityRef) => boolean; run(ref: EntityRef): void | Promise<void> }

interface GNode { id: string; label: string; group?: string; status?: string; weight?: number }
interface GEdge { from: string; to: string; kind: string }
interface GGroup { id: string; label: string; parent?: string }
interface GraphModel { nodes: readonly GNode[]; edges: readonly GEdge[]; groups: readonly GGroup[];
  collapsed: ReadonlySet<string> }
interface Layout { hash: string; positions: ReadonlyMap<string, { x: number; y: number }> }
```

`core` exports `registerRoutes(defs: RouteDef[])`,
`registerEntityAction(a: EntityAction)`, `useSelector<T>(sel, eq?)`,
`useCommand(conn)`, `useCapabilities()` and the selectors for each entity
family. The layout of the normalized store inside `core` is open (owner A2):
features read it only through those exported selectors. `AppShell` (A3)
receives the registered routes and actions as props. `graph` exports
`layeredLayout(m: GraphModel): Promise<Layout>` and `forceLayout`, both keyed
by `Layout.hash` over the structure only.

## 3. Capability flags (Frozen; owner A2)

`Capabilities` is computed in `core` and recomputed only when a connection,
welcome or setting changes.

| Flag | True when |
| --- | --- |
| `host.electron` / `host.browser` | the injected `HostAdapter` |
| `aether.connected` | at least one Aether connection is `ready` |
| `aether.feature.<name>` | the named feature is in that connection's `Welcome.features` |
| `archon.connected` | an archond connection is `ready` |
| `archon.feature.<name>` | as above, from archond's `Welcome.features` |
| `standing.<conn>` | `"local"` \| `"operator"` \| `"observer"` |
| `can.operate.<conn>` | standing is `local` or `operator` |
| `can.host_admin.<conn>` | standing is `local` (each use still needs native confirmation) |
| `noesis.available` | an Aether connection lists `noesis` and `noesis_status.enabled` |
| `noesis.engineering_context` | `noesis.available` and that setting is on |
| `desktop.notifications`, `desktop.dialogs`, `desktop.deep_links`, `desktop.managed_start` | Electron with the matching adapter or setting |

## 4. Design tokens (Frozen names; values open, owner A3)

These are CSS custom properties on `:root`, with `[data-theme="light"|"dark"]`.
Components use tokens only and never write raw colours or durations.

- **Colour:** `--color-bg`, `--color-surface-1`, `--color-surface-2`,
  `--color-surface-3`, `--color-border`, `--color-border-strong`,
  `--color-text`, `--color-text-muted`, `--color-text-faint`,
  `--color-accent`, `--color-accent-contrast`, `--color-focus`,
  `--color-danger`, `--color-warning`, `--color-success`, `--color-info`.
- **Status** (one per state, used by pills, graph nodes and lists):
  `--status-queued`, `--status-dispatching`, `--status-running`,
  `--status-waiting`, `--status-operator`, `--status-blocked`,
  `--status-succeeded`, `--status-failed`, `--status-cancelled`;
  `--pr-open`, `--pr-draft`, `--pr-merged`, `--pr-closed`;
  `--issue-open`, `--issue-closed`;
  `--mastery-unseen`, `--mastery-exposed`, `--mastery-demonstrated`,
  `--mastery-fragile`, `--mastery-misconception`.
- **Type:** `--font-ui`, `--font-prose`, `--font-mono`, `--font-display`;
  `--text-xs`, `--text-sm`, `--text-md`, `--text-lg`, `--text-xl`,
  `--text-2xl`, `--text-3xl`; `--leading-tight`, `--leading-normal`,
  `--leading-prose`; `--weight-regular`, `--weight-medium`, `--weight-bold`.
- **Space and shape:** `--space-0`, `--space-1` … `--space-12` (4 px base);
  `--radius-sm`, `--radius-md`, `--radius-lg`, `--radius-xl`;
  `--elevation-1`, `--elevation-2`, `--elevation-3`; `--focus-ring`.
- **Motion:** `--motion-instant`, `--motion-fast`, `--motion-base`,
  `--motion-slow`; `--ease-standard`, `--ease-emphasized`, `--ease-exit`.
  Under `prefers-reduced-motion: reduce` every duration token is `0ms`.

Fonts are bundled (OFL), never fetched. Focus is always visible through
`--focus-ring`. LearnHarness's "no-ring" rule is not adopted, for
accessibility.

## 5. Event subscription and resync (Frozen; owner A2)

### 5.1 Connections

A connection id is `aether:<instance_id>` or `archon:<host-label>`. An
`instance_id` change on reconnect drops all cached state for the old id. That
includes `last_seq`, transcripts and the outbox; unconfirmed submissions move
to review (the existing Aether behaviour).

### 5.2 Aether

1. On `ready`: `subscribe {after: last_seq ?? null, query: {}, deltas: false}`
   for durable events. Then read `get_attention` and `get_tree` (or
   `list_agents_page` pages) when there is no snapshot.
2. Deltas are opened only for agents on screen, with one subscription per
   connection whose `query.agent` set changes by resubscribing. The client
   uses at most 4 of the daemon's 16 subscriptions.
3. Apply a durable event only if `seq > last_seq`. Applying is idempotent.
4. Deltas are transient. A gap in `invocation_seq`, a lag close (1013) or a
   reconnect means: discard the in-flight stream for that invocation and read
   `get_transcript_page {agent, after: last_entry_seq}`. When the first delta
   seen for an invocation has `invocation_seq > 1` (the subscription opened
   mid-invocation), the client reads the transcript page first and then
   applies only later deltas. The committed `agent.message` always replaces
   the streamed text.
5. `history_expired` or `not_found` on subscribe triggers a full resync:
   subscribe live, buffer events, read the snapshots, apply the buffer.

### 5.3 Archon

1. On `ready`: `subscribe_changes {after: change_cursor ?? null}` and
   `subscribe_feed {after: feed_seq}`. These are two cursors and never mixed.
2. Subscribe to changes first, buffering them. Then read the snapshots
   (`task_graph`, `list_item_summaries`); each carries the `as_of` taken in
   its own read transaction (§1.3). Apply the buffered and live changes with
   `seq > as_of` of the snapshot they update. A watermark read separately
   from its snapshot is never used.
3. A `Change` names an entity and its new state kind. The client updates the
   node in place, and refetches the entity (`get_item`) only when it is on
   screen. Several refetches of one entity within 250 ms collapse into one.
4. `history_expired` means a fresh `task_graph`, then `subscribe_changes {after: as_of}`.
5. With protocol 23 (no `changes` feature) the client falls back to today's
   behaviour: refetch summaries 350 ms after any feed item.

### 5.4 Rendering

Store updates are batched to one per animation frame. Graph layouts recompute
only on structural change (nodes or edges added or removed), never on a
status change. Lists of more than 200 rows and every transcript are
virtualized.

## 6. Noesis service (owners F1–F8)

### 6.1 Settings keys (Frozen; Aether config table `[noesis]`, read by `noesis_status`, written by `noesis_configure`)

| Key | Type, default | Notes |
| --- | --- | --- |
| `noesis.enabled` | bool, `false` | master switch (§8 of the issue) |
| `noesis.engineering_context` | bool, `false` | requires `enabled` |
| `noesis.extraction.sessions` | `"manual"\|"auto"`, `"manual"` | auto requires `enabled`; extract after teaching turns |
| `noesis.extraction.engineering` | `"off"\|"auto"`, `"off"` | auto requires `enabled` and `engineering_context` |
| `noesis.budget.interactive_usd_daily` | f64 ≥ 0, `5.0` | 0 = refuse; estimates count for subscription engines |
| `noesis.budget.background_usd_daily` | f64 ≥ 0, `1.0` | |
| `noesis.budget.background_tokens_daily` | u64, `2_000_000` | |
| `noesis.concurrency.interactive` | 1..=4, `2` | |
| `noesis.concurrency.background` | 0..=3, `1` | 0 = no background jobs |
| `noesis.priority.background` | `"low"\|"normal"`, `"low"` | |
| `noesis.models.strong` / `noesis.models.cheap` | model id or unset | unset = first ready subscription model |
| `noesis.billing.allow_api` | bool, `false` | API providers only when true |
| `noesis.disable.grace_seconds` | 0..=300, `60` | in-flight interactive turn grace on disable |
| `noesis.prompts.overrides` | bool, `false` | when true, a file `<aether data dir>/noesis/prompts/<agent>/<NN-name>.md` replaces the built-in segment of that name |

Writing `enabled`, budgets, billing or `prompts.overrides` is `host_only`.
The other keys are `operate`.

Prerequisites (ARCHITECTURE.md §g): a switch is effective only when its
prerequisites are effective. `noesis_configure` accepts any stored
combination. `noesis_status` returns `settings` (as stored) and `effective`
(after prerequisites). Turning a prerequisite off never rewrites a
dependent's stored value.

### 6.2 Commands (Frozen names and required fields; feature `noesis`)

Every command except `noesis_status` and `noesis_configure` returns
`unavailable {reason: "noesis_disabled"}` while disabled, with no side
effect. While disabled, `noesis_configure` only validates and writes the
`[noesis]` config. A patch setting `enabled = true` is written first, and the
service starts only after the write succeeds; `noesis.db` is opened then and
not before. Mutations take `idempotency_key`.

| Command | Fields → reply | Class |
| --- | --- | --- |
| `noesis_status` | `{}` → `{enabled, settings, effective, jobs_running, spend_today: {interactive: Usage, background: Usage}, disable_policy: {grace_seconds, background: "cancel_at_model_boundary"}, prompt_overrides: string[]}` | observe |
| `noesis_configure` | `{patch: settings subset}` → `noesis_status` | host_only / operate (§6.1) |
| `noesis_create_session` | `{goal: string (1..=2000), mode: "teach"\|"practice", context?: EvidenceBundle, practice_targets?: PropositionId[]}` → `noesis_session` | operate |
| `noesis_send` | `{session, text (1..=20000)}` → `ok` | operate |
| `noesis_answer` | `{session, question: QuestionId, answer: {choices?: string[], text?: string, dont_know: bool, confidence?: 0\|30\|50\|70\|90\|100, thinking?: string}}` → `ok` | operate |
| `noesis_get_session` / `noesis_list_sessions` | `{session}` / `{after?, limit? (1..=200)}` → `noesis_session` / `noesis_sessions {sessions: noesis_session[], next_after?}` | observe |
| `noesis_delete_session` | `{session, graph: "preserve"\|"delete_nodes"}` → `ok` | operate |
| `noesis_graph` | `{query?: string, session?, source?, limit? (1..=5000)}` → `noesis_graph {nodes: Proposition[], chains: Chain[], clusters: Cluster[], truncated: bool}` | observe |
| `noesis_delete_proposition` | `{proposition}` → `ok` (its chains go with it) | operate |
| `noesis_delete_source` | `{source, graph: "preserve"\|"delete_unsupported"}` → `ok`; `conflict` while a job on it runs | operate |
| `noesis_extract` | exactly one of `{session}` / `{source}` → `noesis_job {job: Job}` | operate |
| `noesis_import_source` | `{name (1..=200), artifact: ArtifactId}` → `noesis_import {source: SourceId, job: Job}` | operate |
| `noesis_jobs` / `noesis_cancel_job` | `{state?}` → `noesis_jobs {jobs: Job[]}` / `{job}` → `ok` | observe / operate |
| `noesis_learner` | `{concepts?: ConceptId[], query?: string, limit? (1..=1000)}` → `noesis_learner {concepts: ConceptState[]}` | observe |
| `noesis_learner_evidence` | `{concept, before?: u64, limit? (1..=500)}` → `noesis_evidence {evidence: Evidence[], next_before?: u64}` | observe |
| `noesis_correct_learner` | `{concept, state: ConceptStateKind, reason (1..=500)}` → `ConceptState` | operate |

The conversation itself is the agent's transcript (`get_transcript_page`).

**Sealed fields (Frozen; owner F3, tests F7).** `noesis.ask` `answer_key` and
`explanation`, and `noesis.problem` `solution`, are *sealed*. The runtime
seals them where the tool call is produced and persisted, so every read path
sees the same projection, for every standing including `local`:
- No `tool_input` delta is produced for a sealed tool call. The tool name is
  known when the call starts, so this happens before `invocation_seq` is
  assigned (§1.1) and leaves no gap.
- The persisted tool-call entry stores the input with each sealed field
  replaced by the string `"<sealed>"`. `get_transcript_page`, `get_events`,
  `subscribe`, agent reads, run reports and logs
  therefore never carry the value. The value is kept only in `noesis.db`,
  keyed by question or problem id.
- `thinking` of `subsystem = "noesis"` agents is neither produced as a delta
  nor returned by any transcript read.
- Reveal is only through Noesis objects: `QuestionResult` in
  `noesis.answer_recorded` after the answer is recorded, and `solution` in
  `noesis.practice_concluded` after `conclude`. The transcript stays sealed.
- No command, of any class, reads a sealed value before its reveal. Outside
  the daemon, the plaintext exists only in `noesis.db` and the engine CLI's
  own session files on the host (the user's own files, mode 0600).

**Wire objects (Frozen).** Ids are opaque strings with prefixes `ns_`
(session), `q_` (question), `p_` (proposition), `c_` (chain), `k_` (concept),
`s_` (source) and `j_` (job). Proposition and chain ids are content-derived.

```text
noesis_session = {id, goal, mode: teach|practice, phase: probe|plan|teach|practice, agent: AgentId,
                  plan?: PlanTree, open_question?: Question, active_problem?: Problem, created_at, updated_at}
PlanTree       = {revision: u32, nodes: [{id: slug, title, status: upcoming|current|visited}], edges: [{from, to}]}
Question       = {id, kind: choice|recall|goal, prompt, options?: [{value, label}], multi: bool,
                  collapsed: bool, plan_node?: slug, concepts: ConceptId[], posed_at}
                  // the answer key is never sent before the answer is recorded
QuestionResult = {question, correct?: bool, answer_key?: string[] | string, explanation?}
Problem        = {id, statement, mode: recall|discrimination|composition, targets: PropositionId[], posed_at}
Proposition    = {id, name, statement, aliases: string[], provenance: [{source: SessionId|SourceId, message_ref?}], created_at}
Chain          = {id, premises: PropositionId[], conclusion: PropositionId, provenance, created_at}
Cluster        = {id, session: SessionId, members: PropositionId[]}
Job            = {id, kind: extract_session|extract_source|extract_engineering, target: string,
                  state: queued|running|merging|succeeded|failed|cancelled, stats?: {nodes, chains, dropped_chains},
                  error?: string, usage: Usage, created_at, updated_at}
ConceptStateKind = unseen|exposed|demonstrated|fragile|misconception
ConceptState   = {concept: {id, kind: proposition|plan_node|topic, ref, title}, state: ConceptStateKind,
                  strength: f64 (0..=1), last_success_at?, review_due_at?, calibration?: f64,
                  misconceptions: string[], as_of_evidence: u64}
Evidence       = {id: u64, concept, session?, at, kind: probe_answer|predict_answer|check_answer|recall_judged|
                  practice_conclude|exposure|misconception_observed|operator_correction,
                  outcome: correct|incorrect|dont_know|exposed|observed|corrected,
                  confidence?: 0|30|50|70|90|100, mode?, hint_rung?: 0..=5, miss_class?: cue|discrimination|execution|careless|structural, note?}
```

**Events (Frozen; family `noesis`, durable):**

| Event | Payload |
| --- | --- |
| `noesis.session_created` | `{session: noesis_session}` |
| `noesis.phase_changed` | `{session, from, to}` |
| `noesis.question_posed` | `{session, question: Question}` |
| `noesis.answer_recorded` | `{session, result: QuestionResult}` |
| `noesis.plan_updated` | `{session, plan: PlanTree}` |
| `noesis.problem_posed` | `{session, problem: Problem}` |
| `noesis.practice_concluded` | `{session, problem, outcome: solved\|solved_with_hints\|moved_on\|surrendered\|dismissed, rung?, miss_class?, retest: bool, solution: string}` |
| `noesis.job_updated` | `{job: Job}` |
| `noesis.graph_merged` | `{job, stats}` |
| `noesis.learner_updated` | `{concepts: ConceptId[]}` |
| `noesis.settings_changed` | `{settings, effective}` |

F3 generates every shape in this section, wiring included, in one change.
F2, F4 and F8 fill these shapes and never edit the protocol. A shape change
they need is a CONTRACTS.md edit, made by the current protocol owner (F3,
and G3 after it merges) as an optional field. A required field is never
added later.

### 6.3 Evidence bundle (Frozen; owner F6)

`EvidenceBundle = {title, items: [{ref: {kind: "pr"|"issue"|"item"|"run"|"stage"|"commit"|"file"|"agent", id: string, url?: string}, excerpt: string, status: "verified"|"reviewed"|"preliminary"}]}`.
The total of all excerpts is at most 32 KiB. The frontend assembles it from
reads the user can already make. Noesis stores it with the session and never
fetches more on its own.

### 6.4 Learner model (wire Frozen in §6.2; storage Open, owner F4)

The wire shapes `ConceptState` and `Evidence` are frozen above. The storage
follows NOESIS.md §5: tables `concepts`, `evidence` (append-only) and
`concept_state` (derived). The F4 PR records the final columns here.

### 6.5 Learning data retention (Open; owner F3)

`noesis.db` holds Noesis's own state. The teaching conversations are
transcripts of Aether agents in run `noesis`, and Aether's history retention
keeps and expires them. `noesis_delete_session` deletes the Noesis rows and
the session's evidence bundle. Whether it also purges the Aether transcript
(and how that fits Aether's history invariants) is settled in F3's PR and
recorded here. Until then, the UI states that the conversation remains in
Aether history.

## 7. Memory 2.0 (Open; owners G1–G5)

Direction from ARCHITECTURE.md §h. Proposed shape, to be settled against the
G0 benchmark:

- `MemoryRecord` gains `tier: "working"|"operational"|"architecture"|"history"|"evidence"`,
  `state: "candidate"|"active"|"superseded"|"retracted"`,
  `support: {tasks: u32, independent: u32}`,
  `verification: "inferred"|"observed"|"verified"`,
  `refs: [{kind: "repo"|"path"|"symbol"|"commit"|"issue"|"pr"|"test"|"review"|"doc", value: string}]`,
  `stale?: {reason, since}`.
- Commands (feature `memory_v2`): `memory_retrieval_explain {task|invocation}`
  (host_only), `consolidate_memories {scope, dry_run}` (host_only),
  `memory_export {scope?}` / `memory_import {artifact, dry_run}` (host_only),
  `memory_migration_status` (observe).
- Config `[memory]`: `retrieval.max_items` (default 6), `retrieval.min_score`,
  `retrieval.holdout_rate` (default 0.05), `admission.mode = "candidate"|"immediate"`.
- Archon `Lesson` gains `refs` (items, runs, PRs), `support`, `cluster?`.
  `LessonStats` gains `holdout_lift?: {uses, wins, holdout_uses, holdout_wins}`.
- Ownership stays split: Noesis never reads or writes either store, and
  disabling Noesis changes nothing here.

## 8. Remote configuration keys (Frozen; owners AE-M, AR-R)

| Daemon | Key | Meaning |
| --- | --- | --- |
| Aether | `[remote] mounts` | §1.1 |
| Archon | `[remote] mounted_at` | `"/archon"` when served behind an Aether mount; cookie `Path` follows it |
| Archon | `[remote] trusted_origins` | origins (at most 16, `https://host[:port]` or loopback `http://`) accepted on `Origin`; set by `archon remote setup` from Aether's `public_origins` |
| Archon | device `role` in `remote.json` (format 2) | `"operator"` (default for format-1 records) or `"observer"` |
| Aether | publication guard (AE-M) | adding a mount is refused while the publication is a Funnel or cannot be determined; Funnel is refused while any mount exists; mounted traffic (new requests, upgrades and frames on established connections) flows only under a lease, meaning a successful non-Funnel status read ≤ 5 s old, refreshed every 2 s; when the lease ends (Funnel seen, read failure, or expiry on its own timer if a refresh hangs) every open mounted connection is closed (WebSocket close 1008), new requests get 503, and nothing more is forwarded; `get_remote` reports `mounts_blocked: "funnel" \| "status_unknown"` |
