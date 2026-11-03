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
| Delta ordering | every `delta` frame gains `invocation_seq: u64`, starting at 1 for each `(agent, invocation)` and increasing by 1 per delta frame sent on that subscription. `tool_input` gains `tool_use_id?: string`. Sent only to clients that list `delta_order`. | n/a | `delta_order` |
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
- Feature strings: `changes`, `task_graph`, `run_plan`, `item_history`,
  `item_filters`, `github_links`, `github_import`, `device_roles`.
- Requests from the remote listener run with `ClientKind::ArchonWeb` standing,
  not `ArchonCli` (enforced in AR-R).
- New `ErrorCode::history_expired {first_seq, last_seq}`.
- `archon schema` writes `crates/archon-core/protocol.schema.json` (schemars,
  JSON Schema 2020-12) covering `ClientFrame` and `ServerFrame`. A test fails
  when the file is stale.

**Changes and history (owner AR-C)**

| Item | Shape | Class |
| --- | --- | --- |
| `subscribe_changes` | `{after?: u64}` → `subscribed {id, last_seq}`, then frames `{"frame":"change","sub":id,"change":Change}` | Read |
| `changes_page` | `{after: u64, limit?: u32 (1..=1000)}` → `changes {changes: Change[], last_seq}` | Read |
| `Change` | `{seq: u64, at: Ts, subject: ChangeSubject}`. `seq` is gapless and written in the same transaction as the state it describes. | |
| `ChangeSubject` (tag `subject`) | `item {item, state: QueueStateKind, stage?: string, waiting_reason?: WaitingReasonCode}` · `dependency {item, depends_on, added: bool}` · `run {run, item, state: RunStateKind}` · `stage {stage_run, run, item, stage_id, attempt, fan_index, state: StageStateKind, verdict?: Verdict}` · `gate {run, item, stage_id, decision?: GateDecision}` · `question {question, item?, open: bool}` · `initiative {initiative, state: InitiativeState}` · `link {item, host, repo, number, relation}` | |
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
| `list_items` (extended) | adds `updated_since?: Ts`, `before?: Ts`, `text?: string (1..=200 chars, title and brief substring)` | Read |

`as_of` is the change seq the projection was read at. Nodes are ordered by
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
| `list_github_objects` | `{repo, kind?, state?, limit?: u32 (≤500)}` → `github_objects {objects, synced_at?}` | Read | D2 |
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
| Aether `aether.db` | V29 | AE-P | indexes for paging and `usage_breakdown`, if needed (otherwise an empty slot) |
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
| `web/packages/protocol/` | `@aether-ui/protocol` | A1 | generated `aether.ts` and `archon.ts` types and command builders. No runtime code beyond type guards. |
| `web/packages/core/` | `@aether-ui/core` | A2 | `createApp(host: HostAdapter)`, `ConnectionManager`, `Transport` (`ws-aether`, `ws-archon`, `ipc`), `Store`, `useSelector`, `useCommand`, `Capabilities`, `routes` (§2.3) |
| `web/packages/ui/` | `@aether-ui/ui` | A3 | `tokens.css`, `ThemeProvider`, primitives (Button, Field, Menu, Dialog, Sheet, Tabs, Toast, Tooltip, Badge, StatusPill, Skeleton, EmptyState, ErrorState), `AppShell`, `NavRail`, `CommandPalette`, `VirtualList`, `registerRoutes`, `registerEntityAction` |
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
  privileged(action: {conn: string; frame: unknown; summary: string}): Promise<unknown>; // native confirm
  startDaemon(kind: "aether" | "archon"): Promise<void>;     // only if desktop.managed_start
  pickDirectory(purpose: string): Promise<string | null>;
  notify(n: {title: string; body: string; route?: string}): void;
  openExternal(url: string): Promise<void>;      // https: only
  onDeepLink(cb: (route: string) => void): () => void;       // aether-app://open?route=…
  diagnostics(): Promise<{versions: Record<string, string>; daemons: LocalDaemon[]; logPath: string}>;
  getPreference(key: PreferenceKey): Promise<unknown>;
  setPreference(key: PreferenceKey, value: unknown): Promise<void>;
}
type PreferenceKey = "window.bounds" | "layout.panels" | "theme" | "desktop.managed_start" | "remote.origins";
```

Main validates every argument and size: frames at most 1 MiB, strings at most
4 KiB. It refuses frames whose command is outside the class allowlist
(ARCHITECTURE.md §c), and refuses calls from any frame other than the
top-level `app://` document.

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
   `get_transcript_page {agent, after: last_entry_seq}`. The committed
   `agent.message` always replaces the streamed text.
5. `history_expired` or `not_found` on subscribe triggers a full resync:
   subscribe live, buffer events, read the snapshots, apply the buffer.

### 5.3 Archon

1. On `ready`: `subscribe_changes {after: change_cursor ?? null}` and
   `subscribe_feed {after: feed_seq}`. These are two cursors and never mixed.
2. Snapshots (`task_graph`, `list_item_summaries`) carry or are followed by
   `as_of`. Apply only changes with `seq > as_of`.
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

## 6. Noesis service (owner F1–F6)

### 6.1 Settings keys (Frozen; Aether config table `[noesis]`, read by `noesis_status`, written by `noesis_configure`)

| Key | Type, default | Notes |
| --- | --- | --- |
| `noesis.enabled` | bool, `false` | master switch (§8 of the issue) |
| `noesis.engineering_context` | bool, `false` | requires `enabled` |
| `noesis.extraction.sessions` | `"manual"\|"auto"`, `"manual"` | auto = extract after teaching turns |
| `noesis.extraction.engineering` | `"off"\|"auto"`, `"off"` | requires `engineering_context` |
| `noesis.budget.interactive_usd_daily` | f64 ≥ 0, `5.0` | 0 = refuse; estimates count for subscription engines |
| `noesis.budget.background_usd_daily` | f64 ≥ 0, `1.0` | |
| `noesis.budget.background_tokens_daily` | u64, `2_000_000` | |
| `noesis.concurrency.interactive` | 1..=4, `2` | |
| `noesis.concurrency.background` | 0..=3, `1` | 0 = no background jobs |
| `noesis.priority.background` | `"low"\|"normal"`, `"low"` | |
| `noesis.models.strong` / `noesis.models.cheap` | model id or unset | unset = first ready subscription model |
| `noesis.billing.allow_api` | bool, `false` | API providers only when true |
| `noesis.disable.grace_seconds` | 0..=300, `60` | in-flight interactive turn grace on disable |

Writing `enabled`, budgets or billing is `host_only`. The other keys are
`operate`.

### 6.2 Commands (Frozen names and required fields; feature `noesis`)

Every command except `noesis_status` returns
`unavailable {reason: "noesis_disabled"}` while disabled, with no side
effect. Mutations take `idempotency_key`.

| Command | Fields → reply | Class |
| --- | --- | --- |
| `noesis_status` | `{}` → `{enabled, settings, jobs_running, spend_today: {interactive, background}, disable_policy}` | observe |
| `noesis_configure` | `{patch: settings subset}` → `noesis_status` | host_only / operate (§6.1) |
| `noesis_create_session` | `{goal: string (1..=2000), mode: "teach"\|"practice", context?: EvidenceBundle, practice_targets?: PropositionId[]}` → `noesis_session` | operate |
| `noesis_send` | `{session, text (1..=20000)}` → `ok` | operate |
| `noesis_answer` | `{session, question: QuestionId, answer: {choices?: string[], text?: string, dont_know: bool, confidence?: 0\|30\|50\|70\|90\|100, thinking?: string}}` → `ok` | operate |
| `noesis_get_session` / `noesis_list_sessions` | `{session}` / `{after?, limit?}` → `noesis_session` / `noesis_sessions` | observe |
| `noesis_delete_session` | `{session, graph: "preserve"\|"delete_nodes"}` → `ok` | operate |
| `noesis_graph` | `{query?: string, session?, source?, limit?}` → `{nodes: Proposition[], chains: Chain[], clusters, truncated}` | observe |
| `noesis_extract` | `{session? \| source?}` → `{job}` | operate |
| `noesis_import_source` | `{name, artifact: ArtifactId}` → `{source, job}` | operate |
| `noesis_jobs` / `noesis_cancel_job` | `{}` / `{job}` | observe / operate |
| `noesis_learner` | `{concepts?: ConceptId[], query?: string, limit?}` → `{concepts: ConceptState[]}` | observe |
| `noesis_learner_evidence` | `{concept, limit?}` → `{evidence: Evidence[]}` | observe |
| `noesis_correct_learner` | `{concept, state, reason}` → `ConceptState` | operate |

`noesis_session = {id, goal, mode, phase: "probe"|"plan"|"teach"|"practice", agent: AgentId, plan?: PlanTree, open_question?: Question, created_at, updated_at}`.
The conversation itself is the agent's transcript (`get_transcript_page`).
Events (family `noesis`): `noesis.session_created`, `noesis.phase_changed`,
`noesis.question_posed`, `noesis.answer_recorded`, `noesis.plan_updated`,
`noesis.problem_posed`, `noesis.practice_concluded`, `noesis.job_updated`,
`noesis.graph_merged`, `noesis.learner_updated`, `noesis.settings_changed`.

### 6.3 Evidence bundle (Frozen; owner F6)

`EvidenceBundle = {title, items: [{ref: {kind: "pr"|"issue"|"item"|"run"|"stage"|"commit"|"file"|"agent", id: string, url?: string}, excerpt: string, status: "verified"|"reviewed"|"preliminary"}]}`.
The total of all excerpts is at most 32 KiB. The frontend assembles it from
reads the user can already make. Noesis stores it with the session and never
fetches more on its own.

### 6.4 Learner model (Open; owner F4)

The direction is in NOESIS.md §5: tables `concepts`, `evidence`
(append-only) and `concept_state` (derived). The F4 PR records the final
columns here.

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
