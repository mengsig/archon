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

Mount prefixes never shadow Aether's own paths: `validate_mounts` refuses
`/api`, `/ws` and every prefix the embedded web assets are served under.
`/pair/` (trailing slash) reaches pairing exactly like `/pair`.

### 1.2 Aether protocol 26: later additions (same version, own feature)

| Item | Owner | Status |
| --- | --- | --- |
| Memory 2.0 commands and fields, §7 | G3 | Frozen, settled by G3 (feature `memory_v2`) |

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
  A test fails when the file is stale. Its top level carries
  `protocol_version` and `min_web_protocol_version` (the oldest version
  accepted from `archon-web`). `min_web_protocol_version` is the one name;
  Aether's client generator maps it to its own `min_protocol_version`.

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
| `run_plan` | `{run}` → `run_plan {run, pipeline, revision: string, stages: StageSummary[], edited: bool, settings?: StageSettings[], finished?: bool}`: the frozen pipeline with steer edits applied | Read |
| `StageSettings` | `{key: string (a stage id or `<stage>.review`), choices: ModelChoice[], model: "tier"\|"exact", effort: "default"\|"override", timeout_minutes?: u32, timeout?: "default"\|"override"}` | |
| `list_items` (extended) | adds `updated_since?: Ts`, `before?: Ts`, `text?: string (1..=200 chars, title and brief substring)`, also on `list_item_summaries`. Until `item_filters` is served, a query using them is refused with `unavailable`. | Read |

`run_plan` serves a live run's plan as it resolves now, and a finished run's
plan as archond kept it when the run finished, with `finished: true`; the
store keeps it as it records the run finished, until the run's history is
pruned, and a later retry or steer of the item does not change it. A run
that finished before archond kept plans returns `not_found`, and its plan
is read from its stage attempts.
`settings` lists each stage in order, its reviewers key after it, with the
model choices its next agents start with (`model: "tier"` when they come
from a tier, resolved again at dispatch; `"exact"` for a named model, which
no tier replaces), and whether the effort and time limit are the pipeline's
`default` or an `override` set for the item. `stages` keeps the pipeline's
own defaults. Its
`edited` is true after any steer edit, of every kind, `AssignRoles` and
`AbortStage` included. Today those two leave it false (a known gap).

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
| `DaemonStatus` (extended) | adds `github_outbox?: GithubOutboxPending[]` (at most 20, longest due first) and `github_outbox_pending: u64`. `GithubOutboxPending` is `{token, purpose: "reply"\|"ship", target: "owner/name#N", attempts: u32, next_attempt_at, last_error?, created_at}`: a comment recorded in the V22 outbox and not yet seen posted. | Read | D3 |

### 1.4 Store migrations (pre-assigned, Frozen)

| Store | Version | Owner | Content |
| --- | --- | --- | --- |
| Archon `archon.db` | V19 | AR-C | `changes`, transitions |
| | V20 | D1 | `github_links` plus backfill |
| | V21 | D2 | `github_objects` |
| | V22 | D3 | `github_outbox` |
| | V23 | G5 | lessons 2.0 columns |
| Aether `aether.db` | V29 | AE-P | `agents.subsystem` (nullable text, set once at creation), plus indexes for paging and `usage_breakdown` |
| | V30 | G1 | memory tiers, candidate state, support, provenance refs |
| | V31 | G2 | `memory_stale` and `memory_holdouts` (the stale mark lives in a store table, never in the record body) |
| | V32 | G3 | no-op: moves the version stamp only, with the usual pre-migration backup (§7.3) |

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
- The vendored schema in `web/schemas` is kept honest two ways: the
  schema-hash rule (SYNC records the source commit and SHA-256), and the H1
  harness, which refuses to run when a daemon's hello `protocol_version` or
  `features` differ from the vendored schema.
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
| `web/packages/settings/` | `@aether-ui/settings` | B3 | `routes.ts` |
| `web/apps/web/` | app | A3 | browser entry, `sw.js`, manifest |
| `web/apps/desktop/` | app | E1 `src/{main,preload,bridge,security}`; E2 `src/{native,discovery,lifecycle}`; E3 packaging files | §2.4 |
| `web/e2e/` | tests | H1 `harness/**` and `specs/smoke/**`; H4 `specs/flows/**`; feature specs live under `web/e2e/specs/<feature>/**`. Run by `scripts/e2e.sh`; not part of `make ci`. | |
| `web/perf/` | tests | H2: the frontend performance runner and budgets, with the budgets in `web/perf/budgets.json`. The check fails more than 10% over budget. | |
| `web/dist/`, `web/schemas/` | generated | SYNC | |

A3 creates the empty skeleton (`package.json`, `src/index.ts`) for every
feature package. Each `index.ts` loads its directories' `routes.ts` with
`import.meta.glob`, so feature tasks never edit a shared index.

Open requirement (owner H5, release): CI fails when the committed
`web/dist/` differs from a fresh build. No such job exists yet.

The toolchain is Vite 7, not 8: Vite 8 pulls in lightningcss (MPL-2.0).
Dependency licences are MIT, ISC, BSD or Apache-2.0, with three dev-only
exceptions: minimatch (BlueOak-1.0.0), caniuse-lite (CC-BY-4.0) and axe-core
(MPL-2.0, test-only, for accessibility checks in A3 and in feature-package
tests, §2.2). None of the three may appear in a shipped bundle.

### 2.2 Import rules (Frozen, enforced by lint in A1)

- `protocol` imports nothing. `graph` and `ui` import only React and
  `protocol` types, and `graph` also d3-force. `core` imports `protocol`.
- Feature packages import `core`, `ui`, `graph` and `protocol`, **never each
  other**. They cross over only through routes (§2.3) and the entity-action
  registry.
- Feature-package tests may also import `axe-core` (dev-only, MPL-2.0, for
  accessibility checks), alongside the dev-only licence exceptions of §2.1.
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
  - Patches are limited to known keys (Archon settings), so their full
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
`app.ts` also declares the types §2.4 names, `LocalDaemon` among them.

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

Follow-up requirement: the eslint-tokens rule's `TIMING_KEY` matches only
duration and delay keys. Today it matches more than those.

Fonts are bundled (OFL), never fetched. Focus is always visible through
`--focus-ring`, for accessibility.

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
2. Deltas are opened only for agents on screen, with at most one delta
   subscription per on-screen agent and at most 3 in all, plus the durable one. The client uses at
   most 4 of the daemon's 16 subscriptions.
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

6. Open requirement (owner archond): pausing or resuming the queue on the
   host must emit a change or feed event, so a connected client's paused
   state does not go stale. No such event exists today (found by H1). The
   version rules (§1.5) are unchanged.

### 5.4 Rendering

Store updates are batched to one per animation frame. Graph layouts recompute
only on structural change (nodes or edges added or removed), never on a
status change. Lists of more than 200 rows and every transcript are
virtualized.

## 7. Memory 2.0 (Aether side Frozen, settled by G3; Archon side settled by G5; owners G1–G5)

Direction from ARCHITECTURE.md §h. The Aether side is settled. The canonical
source is `docs/memory/MEMORY-V2.md` in mengsig/aether (commit
`84d897dd67cf9e4eba0f7bc80867db103112e605`); §7.1–7.6 mirror it, and that
file wins if they differ.

This is the settled text of §7 for the Aether side of Memory
2.0 (G1–G4). G3 exposes on the wire what G1, G2 and G4 built. It does not
change admission, retrieval or consolidation. Archon lessons (G5) are not
covered here (see §7.7).

Everything below is in Aether protocol 26 behind the feature string
`memory_v2`. `PROTOCOL_VERSION` does not change. A client that does not
list `memory_v2` in its hello sees every reply exactly as before.

### 7.1 Record standing

`MemoryRecord` gains one optional field, `standing`. It is filled in on the
`memories` and `memory` replies, and only for a client whose hello lists
`memory_v2`. It is never stored in the record body. The store keeps it in
`memory_admission` (V30) and `memory_stale` (V31).

```text
MemoryRecord.standing?: {
  tier: "working" | "operational" | "architecture" | "history" | "evidence",
  state: "candidate" | "active" | "superseded" | "retracted",
  support: { tasks: u32, independent: u32 },
  verification: "inferred" | "observed" | "verified",
  refs: [{ kind: "repo"|"path"|"symbol"|"commit"|"issue"|"pr"|"test"|"review"|"doc", value: string }],
  stale?: { reason: string, since: Ts, revision?: string }
}
```

- `state` is `MemoryState::of(record, stored)`. Superseded and retracted
  come from the record (`superseded_by`, `forgotten_at`). A pending operator
  proposal is a candidate. A memory with no admission row (one written
  before V30's backfill could reach it) is served as G1 derives it: active,
  with no support.
- `support.tasks` counts the distinct supporting tasks, and
  `support.independent` counts their distinct runs. Promotion needs 2
  independent runs (G1).
- `stale` is present when retrieval demotes the memory (G2: half score,
  never hidden). `revision` is the commit whose tree stopped stating the
  memory, when a repository sweep set the mark.
- `verification: "inferred"` is an agent's claim and is never shown as
  fact (ARCHITECTURE §h.6).

### 7.2 Commands

| Command | Shape | Reply | Class | Remote |
| --- | --- | --- | --- | --- |
| `memory_retrieval_explain` | `{task?: TaskId, invocation?: InvocationId}`, exactly one | `memory_retrieval_explain` | host_only | refused to every paired device |
| `consolidate_memories` | `{scope?: MemoryScope, dry_run: bool}` | `memory_consolidation` | host_only | refused to every paired device |
| `memory_export` | `{scope?: MemoryScope}` | `memory_export` | host_only | refused to every paired device |
| `memory_import` | `{artifact: string, dry_run: bool}` | `memory_import` | host_only | refused to every paired device |
| `memory_migration_status` | `{}` | `memory_migration_status` | observe | allowed, observers included |

Each command is listed in `Command::introduced()` as
`Feature("memory_v2")`, classified in `access.rs`, listed in
`docs/remote.md`, and checked against `authz/command_cases.json`.

`dry_run` has no default on either command, so a client must say which it
means. `scope` is the wire `MemoryScope` (`{"scope":"global"}`,
`{"scope":"repository","id":…}`, and so on). It is matched exactly, with the
role scopes nested in a repository. An absent scope means every scope.

#### 7.2.1 `memory_retrieval_explain`

This command reports what retrieval did for each model call of a task, or
for the one call of an invocation. The trace is recorded when the turn is
built, in the snapshot it explains (`InvocationSnapshot.memory_retrieval`),
so it reports what happened then, not what retrieval would do now.

```text
reply memory_retrieval_explain {
  turns: [{
    snapshot: SnapshotId, agent: AgentId, task?: TaskId, turn: u32, at: Ts,
    invocations: [InvocationId],
    traced: bool,                  // false: the snapshot predates G3, or the turn did not retrieve
    injected: [MemoryId],          // what the model was given: the snapshot's memory layer, in render order
    policy?: { max_items: u32, min_score?: f32, holdout_rate: f32 },
    pool: u32,                     // candidates considered
    candidates: [{
      memory: MemoryId,
      outcome: "injected" | "held_out" | "trimmed" | "over_limit" | "cut",
      rank?: u32,                  // 1-based rank among the memories that passed the cutoff
      score?: f32,                 // final score; absent for a cut candidate, which is never scored
      relevance: f32,              // what the cutoff compared with min_score
      stale: bool,                 // demoted at half score
      pinned: bool
    }],
    omitted: { over_limit: u32, cut: u32 }   // candidates past the trace caps
  }],
  truncated: bool                  // a task with more than 256 snapshots
}
```

Outcomes:

- `cut`: the relevance was below `min_score`, so the memory was dropped
  before ranking. Pinned memories are never cut. With no query, nothing is
  cut and `min_score` is absent.
- `over_limit`: the memory passed the cutoff but ranked past `max_items`.
- `trimmed`: the memory was within `max_items`, but the model's memory
  budget had no room for it.
- `held_out`: the memory fit the budget, but the holdout drew it (logged in
  `memory_holdouts`).
- `injected`: the memory is in the snapshot's memory layer.

`injected` is read from the snapshot itself, so the reply always says what
the model was given. A trace entry marked for injection that is not in the
layer is reported as `trimmed`. The trace keeps every memory within
`max_items`, the 16 best over-limit memories and the 16 most relevant cut
memories, and counts the rest in `omitted`.

Errors: `invalid_argument` when neither or both of task and invocation are named;
`not_found` for an unknown task or invocation, or a snapshot that retention
removed.

#### 7.2.2 `consolidate_memories`

This command runs G4's `Runtime::consolidate_memories` with abstraction off.

```text
reply memory_consolidation {
  dry_run: bool, scanned: u32,
  clusters: [{ scope: MemoryScope, survivor: MemoryId, members: [MemoryId],
               outcome: "planned" | "merged" | "skipped", promoted: bool, reason?: string }]
}
```

`planned` means a dry run: nothing was written. `merged` means the members
are now superseded by the survivor, and `promoted` says whether the merged
support made a candidate survivor active. `skipped` carries the reason
(something changed under the plan).

#### 7.2.3 `memory_export` and `memory_import`

```text
reply memory_export { artifact: string, records: u32, digest: string }   // digest: SHA-256 hex of artifact
reply memory_import { dry_run: bool, digest: string, records: u32, inserted: u32, unchanged: u32,
                      conflicts: [{ memory: MemoryId, reason: string }] }
```

Export reads every memory in the scope, including candidates, pending
proposals, superseded memories and forgotten ones. It reads them under
the commit lock, so the artifact is one snapshot of the store. More than
100,000 records, or an artifact over 48 MiB as an escaped JSON string, is
refused as `invalid_argument`, with the advice to export per scope. The byte bound
keeps the reply, and the import that sends it back, inside a 64 MiB frame.

Import compares each entry with the store by memory id:

- An entry the store lacks is inserted exactly as exported, with its
  admission row and stale mark.
- An entry identical to the stored one is `unchanged`.
- An entry that differs from the stored one is a conflict. It is reported
  and never applied: an import never overwrites a local memory.

All inserts go in one commit, with one `memory.imported` event, so an
import lands whole or not at all. Importing the same artifact again inserts
nothing. `dry_run` reports the same counts and writes nothing.

Import errors (`invalid_argument`, in a dry run too, and nothing is written):
- a missing or unknown header;
- a version newer than this build reads;
- a record count that does not match the header;
- a malformed entry, naming its line;
- a repeated memory id;
- an admission naming another memory;
- an admission state other than candidate or active;
- a proposal state the store refuses;
- an entry carrying `standing`;
- an `importance` or `confidence` that is not a finite number in 0..=1
  (a JSON number past f32 would parse as infinity);
- a stale note with a blank reason or a blank revision.

The web import refuses, before sending, a file whose escaped JSON string
is over the same 48 MiB bound, so it takes every artifact an export gives.

#### 7.2.4 Export artifact format

The artifact is UTF-8 JSON Lines, versioned and self-describing.

Line 1 is the header:

```json
{"format":"aether.memory-export","version":1,"exported_at":"2026-11-05T06:00:00Z","schema":32,"scope":null,"records":2}
```

Each following line is one memory, ordered by `created_at` and then by id:

```json
{"record":{…MemoryRecord, without standing…},"admission":{…MemoryAdmission…}|null,"stale":{"reason":"…","since":"…","revision"?:"…"}|null}
```

- `record` is the stored body, every field included: ids, scope,
  provenance, supersession links, proposal, pin, forget and retraction
  state, and retrieval counts.
- `admission` is the stored V30 row: tier, state, the supporting tasks with
  their runs, verification, refs and `promoted_at`. It is `null` when the
  store holds none, and an import then writes none, so the round trip stays
  exact.
- `stale` is the V31 mark, or `null`. Its `revision` is absent when no
  sweep set the mark.

A reader refuses a version it does not know. A later version may add
optional fields. Feedback, the holdout log and retrieval traces are not
exported, because they name tasks and runs of the store that wrote them.

#### 7.2.5 `memory_migration_status`

```text
reply memory_migration_status {
  schema_version: u32,                       // the store's stamp
  migrations: [{ version: u32, name: string, applied: bool }],   // V30, V31, V32
  memories: { total: u32, admitted: u32, implicit: u32, candidates: u32, stale: u32, held_out: u32 },
  backups: [{ name: string, from_version: u32, to_version: u32, bytes: u64, at: Ts }],
  last_import?: { at: Ts, digest: string, inserted: u32, unchanged: u32, conflicts: u32 }
}
```

- `implicit` counts memories with no admission row, which are read as
  active.
- `held_out` counts rows in the holdout log.
- `last_import` is the last applied import. It is kept in the store's meta
  key `memory.last_import` as well as the `memory.imported` event, so the
  status reads it in one lookup.
- `backups` lists the pre-migration snapshots in `migration-backups/`
  beside the store, newest first, by file name only: no host path is sent
  to a device.

### 7.3 Store migration V32

V32 ships empty, so the numbering stays fixed (CONTRACTS §1.4):
- export and import need no bookkeeping table, because idempotency comes
  from comparing ids and bodies;
- the import record is the `memory.imported` event;
- the retrieval trace lives in the snapshot body.

The version stamp still moves from 31 to 32. Opening a V31 store therefore
takes the pre-migration backup `schema-31-before-32-<id>.sqlite.zst`, as
every migration does (docs/migration-backups.md).

**Rollback:** stop the daemon, restore that backup as
docs/migration-backups.md describes, and start the previous binary. The
backup is a V31 store with every row intact. The previous binary refuses
the V32 store itself as `schema too new`. The upgrade lane's `REWINDS`
entry for 32 is empty: only the stamp goes back.

### 7.4 Config

The keys are as G2 shipped them, under `[memory.retrieval]`:

| Key | Default | Range |
| --- | --- | --- |
| `max_items` | 6 | 1–50 |
| `min_score` | 0.05 | 0–1 |
| `holdout_rate` | 0.05 | 0–1 |
| `holdout_seed` | 0 | u64 |

`admission.mode` is not a config key yet. G1 keeps `Immediate` as the
library default, and turning candidates on is a later behaviour change with
its own benchmark.

### 7.5 CLI

| Command | Calls |
| --- | --- |
| `aether memory explain (--task T \| --invocation I) [--json]` | `memory_retrieval_explain` |
| `aether memory consolidate [--scope S] [--dry-run] [--json]` | `consolidate_memories` |
| `aether memory export [--scope S] [-o FILE] [--json]` | `memory_export`; writes the artifact to FILE, or to stdout |
| `aether memory import FILE [--dry-run] [--json]` | `memory_import` |
| `aether memory migration-status [--json]` | `memory_migration_status` |

`--scope` takes what `aether memory list --scope` takes: `global`,
`role:<role>`, `agent:<agent>`, `subtree:<agent>`,
`repo-role:<role>@<repository>`, or anything else as a repository.

`export -o FILE` writes a staging file it creates under a fresh name
beside FILE, then renames it onto FILE, so FILE is never half an artifact. With `--json` it prints the count and digest.

### 7.6 Changes from the proposal

- The six fields are grouped under one optional `standing` object instead
  of sitting flat on `MemoryRecord`. They are present together or not at
  all, they are never written into the stored record body, and a record
  stays one serde struct without `flatten`, which would slow every store
  read.
- `stale` gains an optional `revision`.
- `consolidate_memories.scope` is optional (absent means every scope), and
  `dry_run` is required on both writing commands.
- `memory_export` returns the artifact inline as JSONL text with its
  SHA-256, and `memory_import` takes that text. No artifact id or host path
  is involved, so a browser can download and upload it.
- Import never overwrites: a differing memory is a reported conflict.
- `memory_migration_status`'s reply shape is new (the proposal named the
  command only).
- `retrieval.max_items`, `min_score` and `holdout_rate` live under
  `[memory.retrieval]`, as G2 shipped them. `admission.mode` is deferred.
- V32 is a no-op.

### 7.7 Archon lessons and ownership

- Archon `Lesson` gains `refs` (items, runs, PRs), `support`, `cluster?`.
  `LessonStats` gains `holdout_lift?: {uses, wins, holdout_uses, holdout_wins}`.
  Settled by G5 (V23), all optional on the wire: `refs?: [{kind: "item"|"run"|"pr", value}]`
  (at most 32, a PR by its URL), `support?: u32` (distinct item refs), `cluster?: LessonId`
  (the smallest id of its near-duplicate group). `holdout_lift` appears only when both arms
  have at least 20 scored runs; Archon config `learning.holdout_rate` (0..0.2, default 0).
- Ownership stays split: each store keeps its own owner.

## 8. Remote configuration keys (Frozen; owners AE-M, AR-R)

| Daemon | Key | Meaning |
| --- | --- | --- |
| Aether | `[remote] mounts` | §1.1 |
| Archon | `[remote] mounted_at` | `"/archon"` when served behind an Aether mount; cookie `Path` follows it |
| Archon | `[remote] trusted_origins` | origins (at most 16, `https://host[:port]` or loopback `http://`) accepted on `Origin`; set by `archon remote setup` from Aether's `public_origins` |
| Archon | device `role` in `remote.json` (format 2) | `"operator"` (default for format-1 records) or `"observer"` |
| Aether | publication guard (AE-M) | adding a mount is refused while the publication is a Funnel or cannot be determined; Funnel is refused while any mount exists; mounted traffic (new requests, upgrades and frames on established connections) flows only under a lease, meaning a successful non-Funnel status read ≤ 5 s old, refreshed every 2 s; when the lease ends (Funnel seen, read failure, or expiry on its own timer if a refresh hangs) every open mounted connection is closed (WebSocket close 1008), new requests get 503, and nothing more is forwarded; `get_remote` reports `mounts_blocked: "funnel" \| "status_unknown"` |
