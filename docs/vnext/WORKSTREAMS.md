# vNext workstreams (issue #377, phase 0)

This file breaks phase 1 into single reviewable tasks. Each task is one Archon
queue item, delivered onto its repository's integration PR
([ARCHITECTURE.md §j](ARCHITECTURE.md)). The interfaces the tasks share are in
[CONTRACTS.md](CONTRACTS.md).

## 1. Rules

### 1.1 Ownership

- A task changes only its **owned paths**. Two tasks whose owned paths overlap
  never run at the same time, and the dependency lists below enforce that.
- *Registration files* are the files that every feature touches: protocol
  enums and dispatch, migration lists, the config loader, workspace
  manifests and CI workflows. Each has one owner at a time, and every task that
  touches one depends on the previous owner:
  - `crates/aether-protocol/**` and `clients/**`: AE-P → F3 → G3.
  - Archon `crates/archon-core/src/protocol.rs`: AR-P only. Later tasks add
    optional fields through their own stub handler files.
  - Archon `crates/archon-queue/src/store/schema.rs`: AR-C → D1 → D2 → D3 → G5.
  - The Aether store migration list: AE-P → G1 → G2 → G3.
  - `web/package.json` and `web/package-lock.json`: A1, then only the SYNC
    errand (§1.4). A1 declares the whole phase 1 dependency set: React,
    Vite, TypeScript, Vitest, Playwright, d3-force and the lint tooling. A
    new dependency needs an edit to CONTRACTS.md and is added by the next
    SYNC errand.
  - `web/apps/desktop/` has its own `package.json` and lockfile, owned by E1.
- Migration numbers are pre-assigned (CONTRACTS.md §1.4).

### 1.2 Concurrency

- At most **4 agents** at once on this host, counting items outside this
  plan (for example the G0 baseline item).
- **One heavy job at a time.** Every Rust test run, web build, Playwright run
  and Electron package goes through the host CI slot (`scripts/ci-slot.sh` in
  Archon, and the same wrapper used by the Aether pipeline), so stages queue
  rather than overlap. Tasks marked **exclusive** (end-to-end, performance and
  packaging) never share a wave with another exclusive task.
- The waves in §3 are the default schedule. Archon starts an item as soon as
  its `depends_on` items are merged onto the integration PR and a slot is
  free, so a wave may start early. It never exceeds the limits above.

### 1.3 Sizes and pipelines

| Size | Meaning |
| --- | --- |
| S | ≤ 300 changed lines, excluding generated files |
| M | ≤ 800 |
| L | ≤ 1,500. Larger work is split before filing. |

Pipelines: `adaptive` (default for code), `design-doc` (when a task must
settle an open contract first), `code-audit` (review-only work), `e2e-testing`,
and `performance`.

### 1.4 SYNC errands (not agents)

After each wave, the orchestrator runs one mechanical errand per repository:

1. Merge `next` into the integration branch (merge, never rebase).
2. Vendor the Archon protocol schema into `web/schemas/` with its source
   commit and SHA-256.
3. Regenerate `web/packages/protocol` and `clients/`.
4. Rebuild and commit `web/dist/`.
5. Run `make ci`.

The `web/dist` drift job is required on the integration PR only after its
SYNC errand. A task's own acceptance is `npm run check` (types, lint, unit
tests and build) and does not include the committed bundle.

### 1.5 Issue body template

Each task below gives an issue **title**. Its body is this template, filled
in from the task block:

```text
Part of mengsig/archon#377 (Archon 1.5.0 / Aether 1.3.0, phase 1).
Task <ID> in docs/vnext/WORKSTREAMS.md; contracts in docs/vnext/CONTRACTS.md <sections>.

<Goal>

Owned paths (change nothing else): <Owns>
Depends on: <Depends>
Size: <Size>. Pipeline: <Pipeline>. Deliver onto the <repo> vNext integration PR.

Acceptance:
- <each acceptance line>
- Tests for every new behaviour; `make ci` (Rust) and/or `npm run check` (web) green.
- No change to a frozen contract without a CONTRACTS.md edit approved in review.
```

Aether issues are filed in the Aether repository and reference the Archon
issue by its full name.

## 2. Workstreams

| Id | Workstream | Repository | Tasks |
| --- | --- | --- | --- |
| A | Frontend foundation | aether | A1–A5 |
| B | Aether daemon and screens | aether | AE-P, AE-M, B1–B3 |
| C | Archon daemon and screens | archon (daemon), aether (screens) | AR-P, AR-R, AR-C, AR-G, AR-L, C1, C2 |
| D | GitHub linking | archon (daemon), aether (screens) | D1–D4 |
| E | Electron shell | aether | E1–E3 |
| F | Noesis | aether | F1–F8 |
| G | Memory 2.0 | aether, archon | G0 (existing item), G1–G5 |
| H | Quality and release | both | H1–H5 |

## 3. Waves

| Wave | Tasks (≤ 4 agents) | Exclusive |
| --- | --- | --- |
| 1 | AE-P, AR-P, A1 (+ G0, already running) | — |
| 2 | A2, A3, AR-R, F1 | — |
| 3 | A4, AR-C, AE-M, F2 | — |
| 4 | D1, A5, B1, F3 | — |
| 5 | AR-G, B2, E1, G1 | — |
| 6 | C1, B3, D2, F4 | — |
| 7 | C2, D3, E2, F8 | — |
| 8 | D4, F5, G2, G4 | — |
| 9 | AR-L, F6, G3, H1 | H1 |
| 10 | G5, F7, E3 | E3 |
| 11 | H2, H3 | H2 |
| 12 | H4 | H4 |
| 13 | H5 | — |

Owned paths within each wave are disjoint, and §4 lists them per task. Paths
are relative to the repository root.

## 4. Tasks

### A: Frontend foundation (aether)

#### A1. Web workspace and protocol package
- **Goal:** create the `web/` npm workspace with strict TypeScript, Vite,
  lint, Vitest and the import-boundary lint rules (CONTRACTS.md §2.2). Add
  the generator that turns `clients/protocol.schema.json` and
  `web/schemas/archon.protocol.schema.json` into `@aether-ui/protocol`. Write
  the shared app types of CONTRACTS.md §2.5, verbatim, in
  `web/packages/protocol/src/app.ts`, so that A2, A3 and A4 can start in
  parallel.
- **Owns:** `web/package.json`, `web/package-lock.json`, `web/tsconfig*.json`,
  `web/eslint.config.js`, `web/scripts/**`, `web/schemas/**`,
  `web/packages/protocol/**`, `.github/workflows/web.yml`, and the `web`
  target in `Makefile`.
- **Depends on:** none (uses the schemas at the current HEAD; the SYNC errand
  regenerates them after AE-P and AR-P).
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** `npm run check` passes on a clean clone. The generator is
  deterministic: run twice, the second run gives no diff. A lint test proves
  that a feature package importing another feature package fails. The
  dependency licences are MIT/ISC/BSD/Apache-2.0 only, checked by a script.
  `app.ts` matches CONTRACTS.md §2.5 (a test compares the declarations).
- **Title:** vNext A1: web workspace, toolchain and generated protocol package

#### A2. Core: connections, transports, store, capabilities
- **Goal:** implement `@aether-ui/core` per CONTRACTS.md §2.1, §3 and §5:
  - `HostAdapter`;
  - the `ws-aether` transport (device-key auth reused from the existing PWA);
  - the `ws-archon` transport (cookie auth under `/archon`);
  - the `ipc` transport against the `window.desktop` type;
  - `ConnectionManager` with backoff and the resync rules;
  - a normalized `Store` with per-frame batching;
  - `useCommand` with idempotency keys and role checks;
  - `Capabilities`;
  - the route and entity-action registries (`registerRoutes`,
    `registerEntityAction`) and the exported selectors (§2.5);
  - the hash router;
  - the pairing flow logic.
- **Owns:** `web/packages/core/**`
- **Depends on:** A1
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** unit tests against recorded frame fixtures cover: replay
  after reconnect; dropping an event with `seq ≤ last_seq`; an
  `invocation_seq` gap triggering a transcript reread; an `instance_id`
  change clearing the cache; `history_expired` triggering a full resync;
  observer standing disabling `can.operate`; and the protocol 23 Archon
  fallback. No transport code exists outside `core`.
- **Title:** vNext A2: frontend core (connections, resync, store, capabilities)

#### A3. UI kit, tokens and app shell
- **Goal:** add `tokens.css` with every token name in CONTRACTS.md §4 (light
  and dark), the primitives, `AppShell`, `NavRail`, `CommandPalette`,
  `VirtualList`, with the shell rendering the routes and actions it receives
  as props (CONTRACTS.md §2.5). Add the browser
  entry `web/apps/web` with a service worker and manifest, and empty
  skeletons for every feature package.
- **Owns:** `web/packages/ui/**`, `web/apps/web/**`, and
  `web/packages/{aether,archon,github,noesis,settings}/{package.json,src/index.ts}`
- **Depends on:** A1
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** a token test fails if a name in CONTRACTS.md §4 is missing.
  A lint rule rejects raw colours and durations in components. axe checks
  pass on the shell, and focus is visible on every primitive. Under reduced
  motion every duration token is 0 ms. `VirtualList` renders 10,000 rows with
  fewer than 60 DOM nodes.
- **Title:** vNext A3: design tokens, component library and app shell

#### A4. Graph package
- **Goal:** `@aether-ui/graph`: a Canvas 2D `GraphCanvas` with dirty-flag
  rendering, `Viewport`, a worker-based deterministic `layeredLayout`
  cached by structure hash, `forceLayout` (for the knowledge graph), and
  `GraphModel` with groups and collapse.
- **Owns:** `web/packages/graph/**`
- **Depends on:** A1
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** the same input gives the same coordinates, byte for byte.
  A status-only change causes no relayout (counted in a test). A 2,000-node,
  4,000-edge layered layout finishes in under 1.5 s in the worker. Hit
  testing, keyboard focus and a screen-reader node list are tested.
- **Title:** vNext A4: graph rendering and deterministic layout package

#### A5. Serve the new frontend from aetherd
- **Goal:** embed `web/dist/` in `aether-remote` in place of the legacy PWA:
  CSP `default-src 'self'`, service worker scope, and the pairing page.
  Retire `crates/aether-remote/web/`, moving its mock-daemon and live tests
  into `web/` where they still apply.
- **Owns:** `crates/aether-remote/build.rs`, `crates/aether-remote/src/assets/**`,
  `crates/aether-remote/src/assets.rs`, `crates/aether-remote/web/**`
  (removal), `web/dev/**`
- **Depends on:** A2, A3, AE-M
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** a release build without Node serves the committed bundle.
  The existing remote security tests (Host, Origin, CSP, pairing, revocation)
  pass unchanged. The legacy asset tree is gone, and no route returns it.
- **Title:** vNext A5: serve the unified frontend from Aether's listener and retire the legacy PWA

### B: Aether daemon and screens (aether)

#### AE-P. Aether protocol 26 read models
- **Goal:** implement CONTRACTS.md §1.1: `list_agents_page`, the `get_tree`
  filters, `get_transcript_page`, ordered deltas (`delta_order`),
  `usage_breakdown`, and `RemoteSettings.mounts` (wire and storage only).
  Bump `PROTOCOL_VERSION` to 26 and regenerate the clients.
- **Owns:** `crates/aether-protocol/**`, `clients/**`,
  `crates/aether-daemon/src/{projections.rs,status_reads.rs,usage_breakdown.rs}`,
  delta emission in `crates/aether-daemon` and `crates/aether-runtime`, the
  Aether store migration V29, and `docs/protocol*` in Aether.
  It must not touch G0's paths (`bench/`, `evals/`, `crates/aether-memory/`).
- **Depends on:** none
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - A protocol 25 client sees byte-identical frames (compat test).
  - Paging covers every agent exactly once under concurrent creation.
  - `invocation_seq` is assigned at production: it is gapless per invocation
    across 1,000 deltas, a delta dropped on the bus shows as a gap to the
    subscriber, two subscriptions see the same numbers, and resubscribing
    mid-invocation never renumbers.
  - V29 adds `agents.subsystem`. It is set only at creation, and no command
    changes it.
  - `usage_breakdown` totals equal the sum of `invocation.completed` usage,
    and its rows are deterministic.
  - Each new command has an access class, a feature string, and an
    `introduced()` entry.
- **Title:** vNext AE-P: Aether protocol 26 (agent and transcript paging, ordered deltas, usage breakdown, mounts)

#### AE-M. Listener path mount for archond
- **Goal:** forward `/archon/*` HTTP and WebSocket traffic, byte for byte,
  to a loopback target, as in ARCHITECTURE.md §d. Configuration is host-only
  through the CLI and `configure_remote`. `/api/health` lists the mounts.
- **Owns:** `crates/aether-remote/src/mount/**` (new), the route hook in
  `crates/aether-remote/src/http/**`, mount settings in
  `crates/aether-daemon/src/remote/**`, and the `aether remote mount` CLI in
  `crates/aether/src/`
- **Depends on:** AE-P
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:**
  - The mount refuses non-loopback targets, more than 4 mounts, and prefixes
    containing `.`, `..`, `%` or `\`.
  - It strips hop-by-hop headers, passes `Origin` unchanged, and adds no
    cookie, header or standing. It keeps the head, body, frame and
    connection limits.
  - With the target down it returns 502 without hanging.
  - An observer device on Aether gains nothing on Archon.
  - Funnel in both orders (CONTRACTS.md §8):
    - adding a mount to a Funnel publication is refused, and so is adding
      one when the publication state is unknown;
    - `aether remote setup --funnel` with a mount present is refused, and so
      is keeping an existing Funnel;
    - a Funnel enabled out of band: new mounted connections fail with 503
      within 5 s, and a WebSocket tunnel opened before detection is closed
      (1008);
    - with a tunnel open, a status read that *fails* and one that *hangs*
      each close the tunnel within 5 s;
    - a test shows that no frame reaches archond once the lease has ended.
- **Title:** vNext AE-M: Aether listener path mount for archond (same-origin browser access)

#### B1. Fleet and topology screens
- **Goal:** fleet list (paged, virtualized, filtered by run, role, state and
  model) and the delegation topology graph, with collapse per run.
- **Owns:** `web/packages/aether/src/{fleet,topology}/**`
- **Depends on:** A2, A3, A4, AE-P
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - 5,000 agents scroll at 60 fps in the perf fixture.
  - Topology lays out again only on structural events.
  - Archon-run agents are labelled by role, with or without archond.
- **Title:** vNext B1: fleet and delegation-topology screens

#### B2. Agent and conversation screens
- **Goal:** agent detail and a virtualized transcript with streaming deltas
  and tool calls, including reasoning, attachments, cost and model. It uses
  `get_transcript_page` and the gap fallback.
- **Owns:** `web/packages/aether/src/{agent,conversation}/**`
- **Depends on:** A2, A3, AE-P
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** a 20,000-entry transcript opens in under 500 ms. Deltas
  join their committed message without duplicates. Tool calls pair by
  `tool_use_id`. Markdown rendering is sanitized (a test feeds hostile HTML).
- **Title:** vNext B2: agent detail and conversation screens

#### B3. Controls, inbox, activity, memory and settings screens
- **Goal:** bring every existing PWA operation into the new UI:
  - agent controls: send, steer, cancel, suspend, model and budget;
  - the attention inbox and question answers;
  - activity;
  - memory review: accept and reject;
  - settings: connections, devices, remote and providers.
  Commands go through `useCommand`, and host-only actions are marked.
- **Owns:** `web/packages/aether/src/{controls,inbox,activity,memory}/**`,
  `web/packages/settings/src/**`
- **Depends on:** A2, A3
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** every existing PWA operation has a tested equivalent. An
  observer sees disabled controls with the reason. Submissions that were
  never confirmed reach the existing recovery review.
- **Title:** vNext B3: controls, inbox, activity, memory review and settings screens

### C: Archon daemon (archon) and screens (aether)

#### AR-P. Archon protocol 24 wire contract
- **Goal:** add every CONTRACTS.md §1.3 type and request with its class and an
  `unavailable` stub. This covers: `hello` 23..=24 for `archon-web`; the
  `Welcome` features; `history_expired`; and `archon schema` with a drift
  test. It also creates the stub handler files that the later tasks own.
- **Owns:** `crates/archon-core/src/protocol.rs`,
  `crates/archon-core/protocol.schema.json` (new), the `archon schema`
  command (`crates/archon/src/cmd/schema.rs` plus its registration line),
  `crates/archon-daemon/src/handlers.rs` (dispatch), the new stub files
  `crates/archon-daemon/src/handlers/{changes,graph,github_links}.rs`, and
  the hello logic in `crates/archon-client/src/connection.rs`
- **Depends on:** none
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:**
  - The CLI and daemon still require an exact protocol match.
  - `archon-web` at 23 and at 24 both connect, and 25 is refused with the
    supported range.
  - Every new request has a class and returns `unavailable`.
  - The schema test fails when the file is stale.
  - Schema output is deterministic.
- **Title:** vNext AR-P: Archon protocol 24 wire contract, version range and schema export

#### AR-R. Archon remote roles and standing
- **Goal:** add device roles: `remote.json` format 2, with format-1 records
  loaded as `operator`. Add role-split allowlists, refusal of `hard` pause and
  resume for devices, `ClientKind::ArchonWeb` standing for remote requests,
  `Welcome.device`, `[remote] mounted_at` / `trusted_origins`, and the
  `Path=/archon` cookie.
- **Owns:** `crates/archon-remote/src/{allow.rs,devices.rs,pairing.rs,sessions.rs,store.rs,listener.rs}`,
  `crates/archon-remote/src/{http,ws}/**`, `crates/archon-remote/tests/**`,
  `crates/archon-core/src/remote.rs`, `crates/archon-daemon/src/remote.rs`,
  `crates/archon/src/cmd/remote/**`
- **Depends on:** AR-P
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:**
  - An observer is refused every `Change` request.
  - `pause{hard:true}` from any device is refused.
  - A format-1 file round-trips.
  - With `mounted_at` set, the cookie path is `/archon` and only trusted
    origins are accepted.
  - The existing pairing, revocation, Host and CSP tests still pass.
- **Title:** vNext AR-R: Archon remote device roles, web standing and mount awareness

#### AR-C. Change stream and item history
- **Goal:** add the gapless `changes` table (V19), written in the same
  transaction as each state change. Add `subscribe_changes`, `changes_page`,
  `item_history` and retention.
- **Owns:** `crates/archon-queue/src/store/changes.rs` (new), V19 in
  `crates/archon-queue/src/store/schema.rs`, the change-emitting writes in
  `crates/archon-queue/src/store/{items,runs,gates,initiatives,stacks}.rs`,
  `crates/archon-daemon/src/handlers/changes.rs`, the `as_of` of the
  `list_item_summaries` reply in `crates/archon-daemon/src/read_commands.rs`,
  `crates/archon-queue/tests/changes*.rs`
- **Depends on:** AR-P
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - A property test: for any sequence of store operations, replaying the
    changes reproduces every item's final state kind.
  - Sequence numbers have no gaps after a crash between write and commit
    (injected failure).
  - A cursor older than retention gets `history_expired`.
  - Subscribers that lag are closed, not buffered without bound.
  - `list_item_summaries.as_of` is read in the snapshot's transaction. A test
    commits a change between the snapshot and its reply and shows that the
    client resync of CONTRACTS.md §5.3 loses nothing.
- **Title:** vNext AR-C: gapless Archon change stream and item transition history

#### AR-G. Task graph, run plan and list filters
- **Goal:** add the `task_graph` projection with typed edges and `as_of`,
  `run_plan` with steer edits applied, and the `list_items` filters.
- **Owns:** `crates/archon-queue/src/store/graph.rs` (new), `crates/archon-daemon/src/handlers/graph.rs`,
  the filter code in `crates/archon-queue/src/store/items.rs`, `crates/archon-queue/tests/graph*.rs`
- **Depends on:** AR-C, D1
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** byte-identical output for the same store. `as_of` equals the
  change seq it was read at (a concurrent-write test). A 2,000-item graph is
  read in under 100 ms. A steered run's plan matches the stages it executed.
- **Title:** vNext AR-G: Archon task-graph, run-plan and history-filter read models

#### AR-L. Archon through the Aether mount; retire Archon's web app
- **Goal:** `archon remote setup` makes sure an Aether listener exists. It
  adds the `/archon` mount (in shared or dedicated mode), sets
  `mounted_at` and `trusted_origins`, and publishes one `tailscale serve`
  origin. Remove `crates/archon-remote/web/` and its embedding. A direct
  browser visit to archond's listener gets a short page pointing to the
  Aether origin.
- **Owns:** `crates/archon-remote/{build.rs,web/**}`, `crates/archon-remote/src/assets.rs`,
  `crates/archon/src/cmd/remote/setup*.rs`, and `docs/remote*.md` in Archon
- **Depends on:** AR-R, AE-M, A5, C1
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** setup is idempotent. Funnel is still refused, including
  when the Aether listener it would mount onto is already published through
  a Funnel (setup refuses with Aether's reason). A paired
  browser on the Aether origin reaches both daemons with their own devices.
  The release build carries no Archon UI assets.
- **Title:** vNext AR-L: serve Archon through Aether's mount and retire the legacy Archon web app

#### C1. Queue, item and pipeline screens
- **Goal:** a queue list (filters, priority, waiting reasons, spend); item
  detail with runs, stages, gates, questions and links; and pipeline views
  from `run_plan`, with live updates from the change stream.
- **Owns:** `web/packages/archon/src/{queue,item,pipeline}/**`
- **Depends on:** A2, A3, AR-G
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - Every operation of the current Archon web app has a tested equivalent.
  - The issue, PR and execution states are shown separately.
  - Against a protocol 23 archond the screens degrade to the polled
    behaviour.
- **Title:** vNext C1: Archon queue, item and pipeline screens

#### C2. Task DAG, initiatives and history screens
- **Goal:** the DAG view (layered, grouped by initiative, critical path,
  blocked reasons), initiative pages, and the history explorer from
  `item_history` and the filters.
- **Owns:** `web/packages/archon/src/{dag,initiatives,history}/**`
- **Depends on:** A4, AR-G, C1
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** a 2,000-node DAG keeps 60 fps pan and zoom. A status change
  repaints without relayout. The history view reproduces the transitions of a
  recorded fixture.
- **Title:** vNext C2: Archon task-DAG, initiative and history screens

### D: GitHub linking (archon, aether)

#### D1. Link store and backfill
- **Goal:** add `github_links` (V20), backfilled from item origins and
  `run_pull_requests`. Write links from the watcher, ship and operator paths.
  Implement `item_links`, `issue_items`, `link_issue` and `unlink_issue`, and
  the `ItemSummary.issue` / `pr` fields.
- **Owns:** `crates/archon-queue/src/store/github_links.rs` (new), V20,
  `crates/archon-daemon/src/handlers/github_links.rs`, link writes in
  `crates/archon-daemon/src/github/**` and `crates/archon-queue/src/store/pull_requests.rs`,
  `crates/archon-core/src/github.rs`
- **Depends on:** AR-C
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** the backfill is idempotent and exact on a fixture of
  today's store. Duplicate links are `ok`. Each link change emits a `link`
  change.
- **Title:** vNext D1: GitHub link store, backfill and link requests

#### D2. Read-only GitHub cache and issue import
- **Goal:** add `github_objects` (V21) and a sync worker over `gh api` with
  ETags, intervals and rate-limit backoff. PR observation of linked items no
  longer stops at 14 days. Add `list_github_objects` and `import_issue` with
  the derived key, plus `archon github import`.
- **Owns:** `crates/archon-daemon/src/github_sync/**` (new),
  `crates/archon-queue/src/store/github_objects.rs` (new), V21,
  `crates/archon-daemon/src/handlers/github_links.rs`, `crates/archon/src/cmd/github.rs` (new)
- **Depends on:** D1
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - The sync never issues a mutating `gh` call; a test stubs `gh` and
    fails on any non-GET request.
  - A 304 response does no write.
  - A second import returns the first item.
  - The issue body is stored as untrusted and grants no authority (tested
    against `docs/operator-authority.md` rules).
- **Title:** vNext D2: read-only GitHub object cache and idempotent issue import

#### D3. Outbox and PR state
- **Goal:** add `github_outbox` (V22) for watcher replies and ship comments,
  at most once across restarts. Fill the `PrRef` review and check state from
  the cache.
- **Owns:** `crates/archon-queue/src/store/github_outbox.rs` (new), V22,
  `crates/archon-daemon/src/github/reply.rs`, the comment paths in
  `crates/archon-pipeline/src/ship.rs`, and the PR-state code in
  `crates/archon-daemon/src/github_sync/**`
- **Depends on:** D2
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** a crash injected after the post and before the outbox
  commit does not double-post on restart. No code path closes an issue.
- **Title:** vNext D3: at-most-once GitHub outbox and PR review/check state

#### D4. GitHub screens
- **Goal:** issue and PR pages that join items, runs, Aether agents, PRs and
  outcomes across both connections; an import action; and entity actions on
  `issue` / `pull_request`.
- **Owns:** `web/packages/github/src/**`
- **Depends on:** C1, D2, D3
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** "succeeded", "merged" and "closed" are never shown as one
  another (a fixture test). The screens work with Archon only, and show
  agents when Aether is connected.
- **Title:** vNext D4: GitHub issue and pull-request screens

### E: Electron shell (aether)

#### E1. Main process, preload and validated bridge
- **Goal:** implement `window.desktop` per CONTRACTS.md §2.4 with the
  ARCHITECTURE.md §c security baseline: `app://` protocol, CSP, schema
  validation, class allowlist, and native confirmation for privileged
  actions.
- **Owns:** `web/apps/desktop/{package.json,package-lock.json,tsconfig.json}`,
  `web/apps/desktop/src/{main,preload,bridge,security}/**`, `web/apps/desktop/test/security/**`
- **Depends on:** A2
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - A security test asserts every `BrowserWindow` option, navigation and
    window-open denial, and the permission handler.
  - The bridge rejects oversize frames, unknown commands, host-only or
    remote-admin commands through `request`, `shutdown` through any method,
    privileged actions on a remote connection, and calls from non-top
    frames.
  - Adversarial tests:
    - an action outside the `PrivilegedAction` union is refused;
    - the confirmation text comes from main's templates and daemon identity;
      renderer values appear only as escaped, length-limited argument slots,
      and a hostile argument (newlines, control characters, a misleading
      "Accept memory" device name) cannot change the template text;
    - a settings patch whose consequential change comes after 200 characters
      of other changes is shown in full, every key with its old and new
      value, and a patch too large to show in full is refused;
    - the frame sent equals the frame confirmed.
  - Credentials never cross IPC (grep test on the bridge types).
- **Title:** vNext E1: Electron main process, preload and validated IPC bridge

#### E2. Native integration, discovery and lifecycle
- **Goal:** socket discovery by the CLI rules; a health panel; notifications;
  deep links; folder picker tokens; and optional managed start (detached,
  off by default).
- **Owns:** `web/apps/desktop/src/{native,discovery,lifecycle}/**`, `web/apps/desktop/test/lifecycle/**`
- **Depends on:** E1
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** quitting or crashing Electron leaves a started daemon
  running (process test). Electron never sends `shutdown`. Managed start is
  off by default and needs the setting.
- **Title:** vNext E2: Electron discovery, native integration and detached daemon lifecycle

#### E3. Packaging and updates (exclusive)
- **Goal:** electron-builder AppImage, deb, and dmg/zip, signed when
  credentials exist; an update flow the user confirms; and a CI workflow.
- **Owns:** `web/apps/desktop/{electron-builder.yml,build/**}`, `.github/workflows/desktop.yml`,
  `docs/desktop.md` in Aether
- **Depends on:** E2
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** the packages install and launch in CI (Linux headless smoke
  test). An update never installs without confirmation. Unsigned builds are
  labelled as such.
- **Title:** vNext E3: desktop packaging, signing and confirmed updates

### F: Noesis (aether)

#### F1. Noesis crate, store and dormant lifecycle
- **Goal:** add `crates/aether-noesis` with the settings of CONTRACTS.md §6.1,
  `noesis.db` migration N1, the budget and concurrency accounting, and
  `Noesis::start_if_enabled`. Disabled means nothing is opened, registered or
  spawned.
- **Owns:** `crates/aether-noesis/{Cargo.toml,src/lib.rs,src/settings.rs,src/store/**,src/budget.rs,README.md}`,
  the workspace `members` line in `Cargo.toml`, the `[noesis]` hook in Aether's
  config loader, and `THIRD-PARTY-NOTICES` if a dependency is added
- **Depends on:** none (AE-P is not required)
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:**
  - With the default config, `noesis.db` does not exist after start and
    stop.
  - Budgets refuse at 0, and at the limit they refuse before the model call.
  - The settings round-trip, and invalid values are rejected with useful
    errors.
  - The effective settings follow the prerequisite table of
    ARCHITECTURE.md §g in every combination (an exhaustive test).
  - No LearnHarness source or prompt text is present (provenance note in
    the crate README).
- **Title:** vNext F1: Noesis crate, store and dormant-by-default lifecycle

#### F2. Teaching loop
- **Goal:** add the Prober, Teacher and Practitioner agents with our own
  prompts (NOESIS.md §3); the probe, plan, teach and practice phases;
  structured questions; plan trees; problem and conclusion records; and the
  Noesis-only harness tools.
- **Owns:** `crates/aether-noesis/src/{teach,prompts,tools}/**`
- **Depends on:** F1
- **Size:** L · **Pipeline:** design-doc (settles NOESIS.md §6 parity
  behaviours T1–T12 as tests), then adaptive
- **Acceptance:** the parity tests T1–T12 pass against a faux engine. A
  "don't know" answer never counts as mastery evidence. Every agent runs in
  run `noesis` with a `noesis.*` role.
- **Title:** vNext F2: Noesis teaching loop (probe, plan, teach, practice)

#### F3. Noesis protocol and daemon wiring
- **Goal:** add every CONTRACTS.md §6.2 command, wire object and event behind
  feature `noesis`, with their access classes. Add the daemon wiring that
  starts and stops the service, and the disable policy. Add the single
  runtime admission guard keyed on `agents.subsystem` (ARCHITECTURE.md §g),
  and settle CONTRACTS.md §6.5.
- **Owns:** `crates/aether-protocol/**`, `clients/**`, `crates/aether-daemon/src/noesis/**` (new),
  `crates/aether-runtime/src/noesis_guard.rs` (new), the guard's call sites in
  scheduler dispatch, in the generic agent command handlers and in startup
  recovery, the registration lines in the Aether daemon dispatch, and
  CONTRACTS.md §6.5 (text)
- **Depends on:** AE-P, F1
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:**
  - While disabled, every `noesis_*` command except `noesis_status` and
    `noesis_configure` returns `unavailable{noesis_disabled}` with no store
    change.
  - `noesis_configure` while disabled writes config only, and enabling
    through it opens `noesis.db` only after the write.
  - Toggling on and off at runtime follows the disable policy (grace, then
    cancel).
  - Writing `enabled`, budgets, billing or prompt overrides is `host_only`.
  - With Noesis disabled, `send_message`, `reply_to_question`, `wake`,
    `resume` and `steer` to a Noesis agent start no model call.
  - Every Noesis invocation is charged to the Noesis budgets.
- **Title:** vNext F3: Noesis protocol commands, events and daemon wiring

#### F4. Learner model
- **Goal:** add N2: concepts, append-only evidence, a derived concept state,
  misconceptions, decay, and operator correction (NOESIS.md §5). Settle
  CONTRACTS.md §6.4.
- **Owns:** `crates/aether-noesis/src/learner/**`, the N2 migration, and
  CONTRACTS.md §6.4 (text)
- **Depends on:** F2, F3
- **Size:** M · **Pipeline:** design-doc then adaptive
- **Acceptance:**
  - The derived state is a pure function of the evidence (a rebuild test).
  - Exposure alone never reaches `demonstrated`.
  - A correction is evidence, not an overwrite.
  - The tables are never joined to the proposition graph (schema test).
- **Title:** vNext F4: Noesis learner-mastery model

#### F5. Learning screens
- **Goal:** session list and chat, question widgets, plan tree, practice, the
  knowledge-graph view (force layout), "my understanding" from
  `noesis_learner`, and the Noesis settings with spend.
- **Owns:** `web/packages/noesis/src/**` except `src/understand/**`
- **Depends on:** A3, A4, F3, F4
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:** with Noesis disabled the route shows an off state and
  sends nothing except `noesis_status` (a network-log test). Mastery uses the
  `--mastery-*` tokens and their text labels.
- **Title:** vNext F5: Noesis learning screens

#### F6. Engineering context ("Understand this")
- **Goal:** the frontend builds an `EvidenceBundle` from reads the user can
  already make. Add N3 storage, the entity actions on item, run, PR and
  agent, and opt-in engineering extraction behind both switches.
- **Owns:** `web/packages/noesis/src/understand/**`, `crates/aether-noesis/src/evidence/**`,
  the N3 migration, and `crates/aether-noesis/src/extract/engineering.rs`
- **Depends on:** F5, F8
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** bundles over 32 KiB are refused. Noesis opens no
  connection to archond (a test with a socket guard). Status labels are kept
  through to the teaching prompt.
- **Title:** vNext F6: Noesis engineering context via bounded evidence bundles

#### F7. Off-by-default guarantees and parity suite
- **Goal:** one test per guarantee in ARCHITECTURE.md §g, plus the NOESIS.md
  §6 parity suite and the optional differential harness.
- **Owns:** `crates/aether-noesis/tests/**`, `crates/aether-daemon/tests/noesis_*.rs`,
  `evals/noesis/**`, `web/e2e/specs/noesis/**`
- **Depends on:** F6, H1
- **Size:** M · **Pipeline:** e2e-testing
- **Acceptance:**
  - Each guarantee is its own named test.
  - The bypass tests cover every generic agent command, and a daemon
    restart with a parked Noesis agent while disabled.
  - The differential harness is opt-in and needs a local LearnHarness
    checkout. CI never runs it.
- **Title:** vNext F7: Noesis off-by-default guarantees and behavioural parity suite

#### F8. Knowledge-graph extraction
- **Goal:** extraction from sessions and imported sources: an extractor,
  an auditor, a deterministic merge with stable ids, chain simplification
  and coverage. Implement `noesis_extract` and `noesis_import_source` jobs on
  the background budget.
- **Owns:** `crates/aether-noesis/src/{extract,graph}/**` except `extract/engineering.rs`
- **Depends on:** F3
- **Size:** L · **Pipeline:** adaptive
- **Acceptance:**
  - The merge is deterministic, and the same inputs always give the same
    graph.
  - Cancelling a job keeps the last committed merge.
  - Jobs respect the background cap and the token budget, and never run
    automatically unless `extraction.sessions = auto`.
- **Title:** vNext F8: Noesis knowledge-graph extraction and merge

### G: Memory 2.0 (aether, archon)

G0 is the existing Aether baseline item, which builds the replayable memory
benchmark. Every G task reports the harness delta against G0's baseline in its
PR, and a regression without an explained trade-off blocks it.

#### G1. Tiers and candidate admission
- **Goal:** add `tier` and `state` (candidate, active, superseded, retracted),
  support counts, verification and refs, with promotion rules (Aether store
  V30). Internal only; G3 exposes them on the wire.
- **Owns:** `crates/aether-memory/src/{admission.rs,tiers.rs,proposals.rs}`,
  the V30 migration, `crates/aether-memory/tests/admission*.rs`
- **Depends on:** G0, AE-P
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** existing memories migrate as `active` with their usefulness
  unchanged. A candidate is never injected. Promotion after two independent
  tasks is tested. The harness shows no regression.
- **Title:** vNext G1: memory tiers and candidate admission

#### G2. Retrieval with cutoff and holdout
- **Goal:** candidate pool plus scoring, a minimum-score cutoff, stale-source
  demotion, and an injection holdout with logging (V31).
- **Owns:** `crates/aether-memory/src/{retrieval.rs,index.rs}`, `crates/aether-context/src/**`,
  `crates/aether-runtime/src/memory_feedback.rs`, the V31 migration
- **Depends on:** G1
- **Size:** L · **Pipeline:** performance
- **Acceptance:** the harness shows better precision@k and fewer injected
  tokens on irrelevant tasks, with retrieval p95 within +10% of the baseline.
  The holdout rate is configurable and logged.
- **Title:** vNext G2: memory retrieval scoring, relevance cutoff and holdout

#### G3. Memory 2.0 protocol, export and migration
- **Goal:** add the `memory_v2` feature: record fields, `memory_retrieval_explain`,
  `consolidate_memories`, export and import, and migration status (V32). Add
  the CLI, and extend the memory review screen. Settle CONTRACTS.md §7.
- **Owns:** `crates/aether-protocol/**`, `clients/**`, `crates/aether-daemon/src/memory*`,
  the memory CLI in `crates/aether/src/`, the V32 migration,
  `web/packages/aether/src/memory/**`, and CONTRACTS.md §7 (text)
- **Depends on:** F3 (protocol ownership), G2, G4, B3
- **Size:** L · **Pipeline:** design-doc then adaptive
- **Acceptance:**
  - Export followed by import round-trips exactly.
  - A rollback to the previous binary with the pre-migration backup is
    tested.
  - Explain output matches what was actually injected.
- **Title:** vNext G3: Memory 2.0 protocol, export/import and migration

#### G4. Consolidation
- **Goal:** deterministic near-duplicate clustering that merges support and
  provenance, plus an optional, budgeted, off-by-default abstraction step
  that keeps the members for rollback.
- **Owns:** `crates/aether-memory/src/consolidation.rs` (new),
  `crates/aether-runtime/src/memory_curation.rs`
- **Depends on:** G1
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** the duplicate rate falls on the harness with no loss of
  recall. Clustering is deterministic. A crash mid-consolidation recovers to
  a consistent state.
- **Title:** vNext G4: memory near-duplicate consolidation

#### G5. Archon lessons 2.0
- **Goal:** add lesson consolidation, structured refs (items, runs, PRs) and
  holdout-measured lift in `LessonStats` (V23). The injection budget is
  unchanged.
- **Owns:** `crates/archon-queue/src/store/lessons/**`, `crates/archon-queue/src/store/lessons.rs`,
  V23, `crates/archon-daemon/src/learned/**`, `crates/archon-core/src/lesson.rs`
- **Depends on:** D3 (migration order), G0
- **Size:** M · **Pipeline:** adaptive
- **Acceptance:** existing lessons migrate unchanged. Lift is reported only
  with enough holdout samples (the Wilson bound is kept). Noesis is never
  involved.
- **Title:** vNext G5: Archon lessons consolidation, provenance and holdout lift

### H: Quality and release (both)

#### H1. End-to-end harness (exclusive)
- **Goal:** Playwright against real aetherd and archond with a faux engine,
  in temporary dirs on short socket paths. Smoke specs cover pairing,
  fleet, queue and resync after a daemon restart.
- **Owns:** `web/e2e/{playwright.config.ts,harness/**,specs/smoke/**}`, `scripts/e2e.sh` in Aether
- **Depends on:** A5, B1, C1
- **Size:** M · **Pipeline:** e2e-testing
- **Acceptance:** the run is hermetic: no network, no real credentials, and
  nothing under the user's data or config dirs (a guard test). It passes
  three times in a row.
- **Title:** vNext H1: end-to-end harness with real daemons and a faux engine

#### H2. Performance budgets (exclusive)
- **Goal:** perf fixtures and budgets: 5,000 agents, a 20,000-entry
  transcript, a 2,000-node DAG, the delta rate, and bundle size. CI tracks
  them.
- **Owns:** `web/perf/**`, `.github/workflows/web-perf.yml`
- **Depends on:** B1, B2, C2
- **Size:** M · **Pipeline:** performance
- **Acceptance:** the budgets are written down and enforced. A regression
  over 10% fails.
- **Title:** vNext H2: frontend performance fixtures and budgets

#### H3. Security audit of remote and desktop
- **Goal:** audit the mount, the Archon roles and standing, the Electron
  bridge and CSP, pairing across the shared origin, and Noesis data
  boundaries. Findings are fixed in the owning task, or filed.
- **Owns:** `docs/security/vnext-audit.md` in each repository
- **Depends on:** AE-M, AR-L, E2, F6
- **Size:** S · **Pipeline:** code-audit
- **Acceptance:** each of ARCHITECTURE.md §c and §d has a checked item with
  evidence. No open high-severity finding remains.
- **Title:** vNext H3: security audit of the mount, remote roles and Electron bridge

#### H4. Cross-daemon flows (exclusive)
- **Goal:** e2e flows: Aether only; Aether with Archon; an observer device;
  an issue imported through to a merged PR (stubbed `gh`); Noesis off, then
  on; and the desktop bridge against the same daemons.
- **Owns:** `web/e2e/specs/flows/**`
- **Depends on:** H1, D4, F7, E2
- **Size:** M · **Pipeline:** e2e-testing
- **Acceptance:** all the flows pass three times in a row. Each flow asserts
  the daemon-side state, not only the DOM.
- **Title:** vNext H4: cross-daemon end-to-end flows

#### H5. Release preparation
- **Goal:** bump the versions (Archon 1.5.0, Aether 1.3.0). Write the
  changelogs, the upgrade notes (protocol ranges, migrations, remote setup
  changes) and the docs index. Run the dry-run release scripts.
- **Owns:** version fields, `CHANGELOG.md`, `docs/` index pages, release notes in both repositories
- **Depends on:** every other task
- **Size:** S · **Pipeline:** adaptive
- **Acceptance:** the release scripts pass in dry-run. An upgrade from 1.4.11
  and Aether 1.2.x is tested. Nothing is published, tagged or merged.
- **Title:** vNext H5: release preparation for Archon 1.5.0 and Aether 1.3.0
