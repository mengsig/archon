# vNext architecture decisions (issue #377, phase 0)

Each section gives a decision, the reasons for it from the baseline in
[BASELINE.md](BASELINE.md), and the alternatives it rejects. The interfaces
named here are specified in [CONTRACTS.md](CONTRACTS.md). The work that builds
them is in [WORKSTREAMS.md](WORKSTREAMS.md).

Invariants that every decision preserves (issue §2.2, §12):

1. aetherd is authoritative for agents, tools, messages, history and
   operational memory. archond is authoritative for items, pipelines, queue,
   review gates and delivery. The UI holds caches of daemon state, never
   competing copies.
2. CLIs, TUIs and headless operation keep working unchanged. Closing a window
   or a browser never stops work.
3. Every existing remote guarantee in BASELINE.md §3.2 holds. A browser never
   gains more than its paired device role.
4. UI state, graphs, costs, links and history are deterministic projections of
   daemon state. No model is called to keep the UI current.

## (a) One canonical frontend

**Decision: the one frontend lives in the Aether repository under a new top-level
`web/` npm workspace. It is a single TypeScript application with optional Archon
and Noesis modules (the issue's first option). Its built output `web/dist/` is
committed and embedded by `aether-remote`. The Electron shell lives beside it in
`web/apps/desktop/`. Archon ships no UI bundle: browsers reach archond through
a path mount on Aether's listener (§d), and the desktop app reaches it over IPC
(§c). The new frontend replaces the web apps of both repositories.**

Reasons:

- **Direction of dependency.** Archon already depends on Aether at runtime, since
  it starts or attaches to an aetherd. Aether must work without Archon. A
  frontend hosted by Aether can treat Archon as an optional backend. A frontend
  hosted by Archon would make standalone Aether depend on Archon, which is the
  cycle the issue forbids.
- **No code dependency in either direction.** The Aether build consumes one *data*
  file from Archon: a JSON Schema of the Archon protocol (`archon schema`,
  CONTRACTS.md §1.3), vendored into `web/schemas/` with its source commit and
  SHA-256. The Archon build consumes nothing from Aether. Version skew is
  handled at runtime by the protocol range and `features[]` of each daemon
  (§b), not at build time.
- **One build, one store, one React.** A single bundle typechecks Archon screens
  against the same core, store and component library as the Aether screens. The
  issue rules out "two independent frontend state-management systems". A plugin
  bundle served by archond would need a stable cross-release module ABI and
  shared singletons such as React and the store, across two release trains.
- **Aether already has the machinery.** Aether has generated TS protocol types,
  schema drift gates, device-key auth, the recovery ledger, a virtualized
  transcript and the delegation graph. Archon's app has none of these.
- **Release builds stay Node-free.** `web/dist/` is committed and CI must
  fail when it differs from a fresh build (the job does not exist yet; H5
  adds it). This is the same pattern as `clients/protocol.schema.json`. `cargo build` and `install.sh` stay unchanged.

Stack: TypeScript (strict), Vite, React 19, and plain CSS with design tokens
(CONTRACTS.md §4). There is no CSS-in-JS runtime and no Tailwind, and the
dependencies are MIT/ISC/BSD only. React was chosen over staying vanilla
because the redesign adds dozens of interactive screens, and the existing
vanilla modules (graph layout, virtual lists, transport) wrap cleanly. One
normalized store is exposed through `useSyncExternalStore`; there is no Redux
and no second store. Graph rendering is a Canvas 2D renderer. Layered DAG
layout runs in a Web Worker and is cached by a hash of the graph's structure.
Force layout (d3-force) is used for the knowledge graph only. Nothing lays out
continuously.

Rejected:
- **A separately built bundle vendored into both binaries.** It works, but every
  Archon release would carry a copy of an Aether artifact. Combined use in a
  browser would still need a same-origin answer (§d), which makes the copy
  redundant.
- **A new repository.** Nothing here is compelling enough: Aether-hosting has no
  cycle, and a third repository would add a third release train.
- **The Archon module living in the Archon repository as a runtime plugin.** This
  is the ABI problem above.

Consequence: Archon screen work (workstream C) is reviewed in Aether's
integration PR. Archon's integration PR carries the protocol, remote, GitHub,
lessons and legacy-app removal work. The Archon module degrades to "Archon not
connected" when no archond is reachable.

## (b) Capability negotiation

**Decision: the frontend computes one immutable `Capabilities` value
(CONTRACTS.md §3) from four inputs. Screens read only that value. Daemons stay
the enforcement point: a hidden button is a convenience, never a control.**

1. **Host:** `electron` or `browser`, fixed at boot from the transport adapter
   that was injected.
2. **Connections:** zero or more Aether connections, keyed by `instance_id`, and
   zero or one archond. Each has a negotiated protocol version, `features[]`, and
   a `standing`: `local` (Electron IPC to a Unix socket), `device:operator` or
   `device:observer`. Aether's device role comes from `Welcome.device.role`.
   Archon gains the same role field (AR-R).
3. **Daemon settings that affect the UI:** for example `noesis.enabled`, read
   from `noesis_status`.
4. **Desktop adapters,** present only in Electron: notifications, dialogs, deep
   links, managed start.

Rules:

- *Aether-only versus Aether+Archon:* Archon screens mount only when an archond
  connection is live. Agents of Archon runs show in the Aether fleet either
  way, labelled by role (`archon-run:*`), so nothing is hidden when Archon is
  down.
- *Local versus remote:* `local` standing exists only inside Electron's
  `app://` renderer and only for daemons on the same host. A browser never
  gets it, nor does an Electron remote window (§c, §d).
- *Operator versus observer:* `can.operate.*` is false for observers. Mutating
  controls render disabled with the reason. Requests the daemon refuses
  (`forbidden`) are shown, not retried.
- A feature that a daemon does not advertise is absent, never emulated. The UI
  never polls for a capability the daemon did not announce.

## (c) Electron shell

**Decision: `web/apps/desktop/` is a thin host. It loads the same `web/dist/` from
a custom `app://` protocol. It talks to local daemons only from the main process,
behind a narrow, validated IPC bridge. Its lifecycle is independent of the
daemons.**

Security baseline, enforced by a test that inspects `BrowserWindow` options and
the session:

- `contextIsolation: true`, `sandbox: true`, `nodeIntegration: false`,
  `webviewTag: false`, `webSecurity: true`. The preload exposes one frozen
  object, `window.desktop` (CONTRACTS.md §2.4), through `contextBridge` and
  nothing else.
- Navigation is pinned to `app://`. `setWindowOpenHandler` denies.
  `openExternal` accepts only `https:` URLs. Permission handlers deny by
  default, allowing only notifications.
- The renderer CSP is `default-src 'self'` with `connect-src 'self'`; the
  main process sets it on every `app://` response. The `app://` renderer
  never opens a network connection: it reaches local daemons only through
  IPC, and remote daemons only through a remote window (below).
- Every IPC call is checked in main for: the sender frame URL (must be the
  top-level `app://` document); the payload against the generated JSON
  Schema; the command against an allowlist built from each daemon's access
  classes. The renderer can send typed protocol frames. It can never send raw
  bytes, a socket path, a shell command or a filesystem path, except a path the
  user picked in a native dialog, returned as an opaque token.
- Commands are filtered by class. Aether `observe` and `operate`, and Archon
  `Read` and `Change` within the remote allowlist, pass through `request`.
  Aether `host_only` and `remote_admin`, and Archon `OperatorRead` and
  `OperatorChange`, never pass through `request`. They are reachable only as
  the closed set of typed `privileged` actions in CONTRACTS.md §2.4, and only
  on a local connection. For each action, main builds the daemon frame itself
  from the validated action data. It also writes the native confirmation
  text, naming the daemon (label and socket), the action and its
  consequential arguments, for example "Pair device 'phone' as observer on
  aetherd (/run/user/1000/aether/aetherd.sock)". The renderer supplies no
  frame and no text, and confirmation is bound to that exact request. An
  action not in the set is refused, whatever its class. `shutdown` is never
  sent, by either method. A compromised renderer therefore cannot silently
  use host standing, nor mislabel what it asks for.
- Credentials never cross IPC. Model logins stay in the engine CLIs (§i).

Discovery and lifecycle:

- Main finds sockets by the same rules as the CLIs: `$AETHER_SOCKET`, then
  `$XDG_RUNTIME_DIR/aether/aetherd.sock`, then the data-dir path; `$ARCHON_SOCKET`
  and the Archon paths likewise. It shows a connection and health panel from
  `status` / `ping`.
- **Optional managed start** is off by default (`desktop.managed_start`). When the
  user asks, main runs `aether daemon` or `archon daemon start` **detached**
  (new session, stdio to the daemons' own logs, `unref`). The daemons are never
  Electron's children, so quitting Electron, closing windows or crashing the
  renderer stops nothing. Electron never sends `shutdown`.
- **Remote daemons open in a remote window, never in the `app://` renderer.**
  Reason: the remote listeners admit only their own HTTPS origins (Origin and
  `Sec-Fetch-Site` checks) and Archon's device cookie is `SameSite=Strict`, so
  an `app://` page cannot authenticate to them without weakening those
  checks. A remote window is a separate `BrowserWindow` that loads the remote
  listener's own `https://` origin (one entry of the `remote.origins`
  preference), which serves the same `web/dist/` (task A5). It is the browser
  case exactly: same origin, device key in that origin's IndexedDB, Archon
  cookie under `/archon`, every existing Origin, cookie and CSP check
  unchanged. Its rules:
  - no preload and no `window.desktop`, so `host.browser`, no `local`
    standing and no privileged actions; it has the same baseline options
    (`contextIsolation`, `sandbox`, no Node, `webSecurity: true`);
  - its own persistent session partition per origin
    (`persist:remote:<sha256(origin)>`), never shared with `app://` or with
    another origin; removing the origin clears that partition;
  - navigation and redirects are pinned to that exact origin; anything else
    is denied or sent to `openExternal`; `setWindowOpenHandler` denies;
  - permission handlers deny everything except notifications; certificate
    errors are never overridden;
  - main never injects credentials, headers or scripts into it and keeps no
    IPC channel to it.
  Local IPC methods are bound to local connection ids only, and Electron
  never forwards anything from a remote window into a local socket.

Packaging: electron-builder produces an AppImage and a deb for Linux, and a
dmg/zip for macOS (signed and notarized when maintainer credentials exist,
unsigned otherwise). Updates use electron-updater against the Aether GitHub
release, and the user always confirms them. Windows is investigated, not
claimed.

## (d) Remote access and Tailscale

**Decision: browser access to both daemons goes through one origin, Aether's
listener. Aether gains a generic, host-configured mount that forwards
`/archon/*` (HTTP and WebSocket) byte for byte to archond's loopback listener.
Each daemon keeps its own pairing, device identity, roles, Host/Origin checks
and revocation. The mount adds no credential and no standing.**

Why one origin: the browser app needs the Aether device key (IndexedDB) and the
Archon device cookie in the same origin. Two origins would need CORS on pairing
endpoints, cross-origin key storage, and a relaxed `Sec-Fetch-Site`. Each of
those weakens an existing check. One mount keeps `connect-src 'self'` and every
current Origin rule intact.

Mount rules (AE-M):
- Configured only by host-only commands or the CLI
  (`[remote] mounts = [{prefix = "/archon", target = "http://127.0.0.1:8378"}]`).
  Targets must be loopback. At most 4 mounts. Prefixes are fixed lowercase
  segments, matched after the existing path normalization rejects `.`, `..`, `%`
  and `\`.
- The mount forwards method, path (prefix stripped), body and WebSocket frames,
  strips hop-by-hop headers, and sets `Host` to the target. It passes `Origin`
  unchanged. It keeps the existing head, body, frame and connection limits.
- archond accepts the mounting listener's origins only when it is told it is
  mounted (`[remote] mounted_at = "/archon"` plus `trusted_origins` copied from
  Aether's `public_origins` by `archon remote setup`). It then scopes its cookie
  to `Path=/archon`.
- `archon remote setup` makes sure an Aether listener exists. In shared mode
  this is the operator's aetherd; in dedicated mode it is Archon's own aetherd,
  whose agents are the ones Archon runs. It adds the mount and publishes one
  `tailscale serve` origin.
- **A Funnel configured through Aether never reaches a mount, and an
  out-of-band Funnel reaches it for at most 5 s.** Archon refuses Funnel
  today, and the mount must not route around that, so Aether enforces both
  orders of operation, plus a status lease:
  - Adding a mount is refused while Aether's publication is a Funnel, or
    while the publication state cannot be determined.
  - `aether remote setup --funnel`, and keeping an existing Funnel
    publication, are refused while any mount exists.
  - **Status lease.** Mounted traffic flows only while Aether holds a
    lease: a *successful* status read showing no Funnel, at most 5 s old.
    This covers new HTTP requests, WebSocket upgrades, and every frame on
    an established connection. The lease is refreshed every 2 s while a
    mounted connection is open, and before admission otherwise.
  - When the lease ends, Aether **closes every open mounted connection** at
    once: keep-alive HTTP, and upgraded WebSockets with close code 1008. A
    lease ends when a Funnel is seen, when a read fails, or when it expires
    because a refresh hangs. Expiry runs on its own timer, independent of
    the refresh. New requests get 503. No byte is forwarded to archond
    without a live lease, and `get_remote` reports why.
  - Residual risk: a Funnel enabled by hand outside Aether is exposed for at
    most the 5 s status age before every mounted connection closes. Funnel
    and serve share one hostname, so Origin checks cannot tell them apart.
    Funnel set up through Aether never reaches a mount.
  `archon remote setup` checks `get_remote` before it adds the mount, and
  refuses with the same reason.

Changes to Archon's remote layer (AR-R):
- Device `role` (`operator` | `observer`) is chosen by the host when pairing.
  `remote.json` gains a format version. Old files load as `operator`, matching
  today's behaviour.
- Allowlists are split per role. `pause`/`resume` with `hard:true` is refused
  for devices, which closes the BASELINE.md §3.2 gap. New read models are
  observer-visible. New mutations are operator-only.
- Web requests execute with a new `ClientKind::ArchonWeb` standing rather than
  CLI standing, so handlers can refuse host-only paths themselves.

Preserved unchanged: pairing codes and phrases, device keys and cookies,
revocation close codes, the Host anti-rebinding check, CSP, Funnel refusal,
idempotency receipts, `resolve_submission`, and private-by-default (both
listeners off until enabled). A remote browser cannot reach a Unix socket:
sockets are reachable only from local processes (CLIs, the Electron main
process).

## (e) Protocol extensions

All are deterministic read models or event streams computed from store state.
None calls a model. Each is specified in CONTRACTS.md §1.

**Aether, protocol 26 (task AE-P):**
- *Fleet:* `list_agents_page` (`after`, `limit`); `get_tree` gains `root` and
  `role_prefix` filters. The delegation graph is built from
  `get_tree` plus `agent.created`, `agent.*` and `parent_agent_id`.
- *Transcripts:* `get_transcript_page` (`after` or `before`, `limit`). Ordered
  deltas: every `Delta` gains `invocation_seq`, assigned once when the delta is
  produced (gapless per invocation, the same on every subscription), and
  `tool_input` gains `tool_use_id`, behind feature `delta_order`. The UI can
  then drop duplicates and detect gaps, falling back to `get_transcript_page`.
- *Costs:* `usage_breakdown{scope, by: model|provider|role|day, since, until}`,
  aggregated from `invocation.completed` usage. It keeps `cost_complete`,
  `cost_gap` and `estimated_cost_usd` semantics; subscription dollars stay
  labelled as estimates.
- *Mounts:* `get_remote` and `configure_remote` gain `mounts`.
- *Noesis* and *Memory 2.0* additions land later in the same version, each
  behind its own feature (§g, §h).

**Archon, protocol 24 (task AR-P):**
- *Negotiation:* `hello` accepts 23..24. `Welcome` gains `min_protocol_version`
  and `features[]`. CLI↔daemon stays exact-match, because they ship as one
  binary.
- *Schema:* `archon schema` emits `crates/archon-core/protocol.schema.json`
  (schemars), with a drift test.
- *Change stream:* `subscribe_changes{after}` replays and then streams `Change`
  records (item, run, stage, gate, question, initiative, link) from a new
  gapless `changes` table, written in the same transaction as the state change.
  This is also the item transition history: `item_history{item}`.
- *DAG:* `task_graph{filter}` returns nodes (item summary subset, status,
  stage, waiting reason, cost, links) and typed edges (`depends_on`,
  `initiative_after`, `stacked_on`), plus `as_of` change seq.
- *Pipelines:* `run_plan{run}` returns the frozen, steer-edited stage list of
  a run with each stage's resolved models, effort and time limit, so steered
  runs render as they actually executed. A finished run's plan is the one
  kept as it finished; one that finished before plans were kept returns
  `not_found`, and its plan is read from its stage attempts.
- *History:* `list_items` gains `updated_since`, `before` and `text` filters.
  `item_history` returns transitions with timestamps.
- *GitHub:* see §f.

**Version rules:** each daemon's version is bumped once, by the first protocol
task of this release (AE-P → 26, AR-P → 24). Later additions within the same
unreleased version are optional fields, or new commands behind a new feature
string. A shipped shape is never changed. Compat rewriting for older clients
stays in Aether's `compat.rs`.

## (f) GitHub issue ↔ task ↔ agents ↔ PR ↔ outcome

**Decision: Archon owns a normalized link store and a deterministic, read-only
GitHub sync. GitHub stays the owner of issue and PR data; Archon caches it and
never edits it implicitly.**

- **Links (D1):** a `github_links` table:
  `(item, host, repo, number, kind: issue|pull_request, relation: origin|imported|delivered|closes|mentioned, created_at, source)`,
  unique on `(item, host, repo, number, relation)`. A migration backfills
  `origin` rows from `items.data.origin` and `delivered` rows from
  `run_pull_requests`. Index `(host, repo, number)` for issue→items lookups.
- **Cache (D2):** `github_objects(host, repo, number, kind, title, state,
  state_reason, labels, author, updated_at, etag, fetched_at,
  pr{draft, merged_at, review_decision, checks})`. A sync worker refreshes linked
  objects, and optionally lists the open issues of registered repositories. It
  uses `gh api` with ETag conditional requests, at most one request per object
  per interval (default 5 min, minimum 60 s), and backs off on rate-limit
  headers. It never writes to GitHub. PR observation for linked items is no
  longer cut off at 14 days; it stops once the PR is merged or closed.
- **Import (D2):** `import_issue{ref}` (operator change) creates an item
  whose brief is the issue body marked as untrusted. Its idempotency key is
  derived as `github:<host>/<owner>/<repo>#<number>`. A second import returns the
  same item. Issue text never grants authority. Authority still comes only from
  the existing watcher and operator rules (`docs/operator-authority.md`).
- **Updates to GitHub (D3):** only the existing paths: watcher replies
  (`REPLY_MARKER`), ship and PR bodies. A `github_outbox` keyed
  `(item, run, purpose)` guarantees at-most-once comments across restarts.
  Closing an issue is never automatic in this release.
- **Distinct states:** the UI shows three separate facts: item execution
  (`QueueState`), PR state (open/draft/merged/closed plus review and checks),
  and issue state (open/closed with reason). "Succeeded" is never shown as
  "merged", and "delivered" is never shown as "closed".
- **Agents:** item → runs → `aether_run`, plus `StageRun.aether_agent` and
  `StageDelegation.agents`, link to Aether agents. The frontend joins these
  client-side across the two connections. Neither daemon stores the other's
  state.

## (g) Noesis

**Decision: reimplement LearnHarness's architecture and behaviour, clean-room, as
`crates/aether-noesis` in Aether. It is a dormant in-process service whose
learning agents are ordinary Aether agents in a dedicated `noesis` run with
their own budget. NOESIS.md gives the licensing analysis, the parity plan and
the learner model.**

Reasons: the teaching loop needs model calls, subscription engines, budgets,
transcripts, question round-trips and persistence. Aether already has all of
them. A Node sidecar would duplicate auth (and LearnHarness has no Codex path),
put an AGPL process in the shipped product, and keep a single-process
JSON-file store. Placing Noesis in Aether rather than Archon keeps it usable
with Aether alone (issue §7.4).

Process model:
- Noesis owns a separate SQLite file, `noesis.db` in Aether's data dir. Aether
  memory never reads it and Noesis never writes Aether memory (§h). Deleting
  the file removes Noesis's own state: graph, learner model, plans, evidence
  bundles and jobs. The conversations themselves are transcripts of Aether
  agents in run `noesis`, kept and expired by Aether's history retention
  (CONTRACTS.md §6.5).
- Prober, Teacher, Practitioner, Extractor and Auditor run as Aether agents with
  roles `noesis.<agent>`, in run `noesis`, with Noesis-only harness tools.
  Interactive turns and background jobs have separate concurrency caps and
  daily budgets, and background jobs use the scheduler's lowest priority. The
  engineering queue is never blocked: Noesis agents count against the Noesis
  caps, not against Archon's.
- Engineering context comes in only as an explicit, bounded **evidence bundle**:
  references and excerpts at most 32 KiB, each marked `verified`, `reviewed`
  or `preliminary`. The frontend builds it from Archon and Aether reads when
  the user presses "Understand this". Noesis never connects to archond.

Off-by-default guarantees (issue §8), each with a test in task F7:
- `noesis.enabled = false` is the default. While disabled:
  - no Noesis harness tool is registered, and no `noesis.*` role can be
    created;
  - no worker or timer starts;
  - `noesis.db` is not opened or created;
  - no context layer is added to any agent;
  - every `noesis_*` command except `noesis_status` and `noesis_configure`
    returns `unavailable{reason: "noesis_disabled"}` with no side effect.
  `noesis_configure` is the host-only control path. While disabled it only
  validates and writes the `[noesis]` config. Setting `enabled = true` then
  starts the service, which opens `noesis.db` only at that point.
- **One runtime guard covers every path to execution.** Noesis agents carry a
  persisted `subsystem = "noesis"` attribute, set at creation and never
  changed; the role label is not trusted for this. Aether's runtime checks
  that attribute in one admission guard before:
  - every scheduler dispatch, meaning every model call;
  - the generic agent commands (`send_message`, `reply_to_question`, `wake`,
    `resume`, `steer`, model changes);
  - delegation from a Noesis agent;
  - startup recovery.
  While Noesis is disabled or draining, the guard refuses new work for these
  agents and parks them, so nothing resumes them later by surprise. Every
  Noesis invocation is charged to the Noesis budgets on the same path. F3
  owns the guard. F7 tests each generic command, and a daemon restart with
  a parked Noesis agent.
- **Answers stay hidden on every read path.** Noesis agents are ordinary
  agents, so the generic transcript, event and delta reads would otherwise
  show the committed answer key or solution in their tool calls. The
  runtime seals those fields where the call is produced and persisted
  (CONTRACTS.md §6.2), before any client, local or remote, can read them;
  they are revealed only through Noesis objects after the answer. Residual
  risk: the model can still restate an answer in its visible text; the
  prompts forbid it and parity test T5 checks it. F3 owns the seal and F7
  tests it.
- Engineering context and the two automatic extraction modes are further
  switches, all off by default. Their dependencies are:

  | Switch | Requires |
  | --- | --- |
  | `noesis.engineering_context` | `enabled` |
  | `noesis.extraction.sessions = "auto"` | `enabled` |
  | `noesis.extraction.engineering = "auto"` | `enabled` and `engineering_context` |

  A switch is effective only when its prerequisites are effective. Turning a
  prerequisite off turns its dependents off in effect, but keeps their stored
  values, and `noesis_status` reports both.
- Disabling refuses new work at once. It cancels background jobs at their next
  model-call boundary (each job's last committed merge stays). It lets an
  in-flight interactive turn finish for up to 60 s and then cancels it. The
  cancellation policy is reported in `noesis_status`.
- Opening the Learning screen triggers nothing. Extraction runs only on an
  explicit action, or when the matching auto switch is on.

## (h) Memory 2.0

**Decision: evolve, do not replace. Keep Aether memory and Archon lessons as
separate stores with their own owners. Improve them against a benchmark that
exists before any behaviour change. Noesis's proposition graph is not used as
coding-agent memory.**

Benchmark first: the Aether baseline item (G0, running in parallel) builds a
replayable harness over recorded tasks with labelled relevant memories. It
measures retrieval precision/recall@k, stale and incorrect influence, duplicate
and low-value rate, injected tokens per turn, retrieval latency (p50/p95),
storage size, and recovery after a crash mid-consolidation. Every Memory 2.0
task must report the harness delta against that baseline. A change that
regresses a metric without an explained trade-off does not ship.

Direction (each item is a task in workstream G, behind the `memory_v2` feature):
1. **Hierarchy:** `MemoryRecord.tier`
   (`working | operational | architecture | history | evidence`), derived at
   admission. Only `operational` and `architecture` are injected by default.
   `history` and `evidence` are retrieved on demand. Ephemeral working context
   never becomes durable.
2. **Admission as candidate:** agent memories start as `candidate`. They are
   promoted when reinforced by two independent tasks, accepted by the operator,
   or verified by an outcome. Persist broadly, inject sparingly.
3. **Consolidation:** deterministic near-duplicate clustering (token and trigram
   similarity, same scope) merges support counts and provenance. Optional
   model-written abstraction of a cluster is budgeted, off unless configured,
   and always keeps the members for rollback.
4. **Retrieval:** a candidate pool (FTS5, trigram, structural: repository, role,
   paths and symbols touched) is scored on scope, tier, usefulness, freshness,
   provenance and verification, and support. A minimum-score cutoff means
   irrelevant tasks get fewer memories, not always 6. Memories whose source file
   changed since their commit are demoted as possibly stale.
5. **Usefulness from evidence:** keep operator and review feedback. Add outcome
   feedback with a **holdout**: about 5% of eligible injections are withheld at
   random and logged, so lift is measured as a difference rather than inferred
   from correlation.
6. **Provenance and verification:** structured refs (repo, path, symbol, commit,
   issue, PR, test, review) and `verification: inferred | observed | verified`.
   Inferred claims are never shown as facts.
7. **Migration and rollback:** additive columns with defaults; a pre-migration
   backup (already in Aether); `aether memory export` / `import` to JSONL; and
   a documented rollback (restore the backup with the previous binary).
8. **Archon lessons:** the same near-duplicate consolidation, structured
   provenance (items, runs, PRs), and holdout-measured lift in `LessonStats`.
   The injection budget (6 lessons / 1,200 bytes) stays until the harness shows
   that a change helps.

## (i) Subscription-first authentication

**Decision: every model call, including Noesis, goes through Aether's existing
engines and providers. Subscription engines (Claude Code, Codex) come first. API
billing is never a silent fallback.**

- Noesis model slots (`noesis.models.strong|cheap`) default to the first *ready*
  subscription engine model reported by `list_models` / `auth_status`. If none
  is ready, Noesis reports `no_model` with the available alternatives. It does
  not pick an API provider unless `noesis.billing.allow_api = true`.
- Credentials stay where they are today: the engine CLIs own subscription
  logins, and API keys stay in Aether's credential store or the environment.
  The frontend shows readiness, limits and headroom from `providers` and
  `auth_status`, which return no secrets. Login is a host action: CLI, or a
  native-confirmed IPC call that opens a terminal.
- Spending is attributed by subsystem through run and role
  (`usage_breakdown{by: role}`): engineering, Archon, Noesis interactive, Noesis
  background.

## (j) Integration and branch strategy

**Decision: one integration PR per repository. Archon's is this PR, whose branch
is the Archon integration branch. Aether's is seeded by the Memory 2.0 baseline
item (G0). Each workstream task is an Archon queue item delivered onto its
repository's integration PR (`onto` the PR), reviewed, with its tests green,
before the next dependent task starts.**

- Items carry `depends_on` from WORKSTREAMS.md. One initiative per wave
  groups the cross-repository items. The contract text of the initiative is the
  pointer to CONTRACTS.md.
- Integration branches follow `next` by **merge** only. One errand per wave
  merges `next` into each integration branch, then runs `make ci`. Nobody
  force-pushes or rebases a shared integration branch.
- A task changes only its owned paths (WORKSTREAMS.md). Shared registration
  points (protocol enums, migration lists, CI scripts) have one named owner
  per wave, and the migration numbers are pre-assigned (CONTRACTS.md §1.4).
- The integration PRs stay drafts and are **not merged** until the release is
  complete. The operator merges and releases (issue: no automatic merge, tag
  or publish).
- Versions are bumped only by the release task H5: Archon 1.5.0 and Aether
  1.3.0, with changelogs.
