# vNext baseline (issue #377, phase 0)

This file records the state that the Archon 1.5.0 / Aether 1.3.0 unified-platform
release starts from. It also lists what already exists and must be reused
rather than rebuilt. Every fact here was read from source at the commits
below, recorded on 2026-11-03. Nothing was built or run.

Companion documents: [ARCHITECTURE.md](ARCHITECTURE.md),
[CONTRACTS.md](CONTRACTS.md), [WORKSTREAMS.md](WORKSTREAMS.md).

## 1. Repositories

| Repository | Default branch | Baseline commit | Version | Integration base | vNext target |
| --- | --- | --- | --- | --- | --- |
| [mengsig/archon](https://github.com/mengsig/archon) (MIT) | `main` | `29c9d2fda49a16c1d89eb197f7139accef125f2c` (tag `v1.4.11`) | 1.4.11 | `next` at `9166c62d0d2c54922f61ec63a758eb2b7bb28ede` | **1.5.0** |
| [mengsig/aether](https://github.com/mengsig/aether) (MIT) | `main` | `ad7d94b809b6cdfebee3c83bc580fe71690bc04f` (tag `v1.2.11`) | 1.2.11 | `next` = `main` = `ad7d94b8` | **1.3.0** |

A 1.4.12 Archon patch release is being prepared in parallel. It does not
change the 1.5.0 target. When it lands on `next`, the integration branch takes
it in by a merge, never a rebase or force-push (ARCHITECTURE.md §j).

Protocol and schema versions at the baseline:

| Surface | Version | Where |
| --- | --- | --- |
| Aether daemon protocol | `PROTOCOL_VERSION = 25`, serves 17..25 | `crates/aether-protocol/src/lib.rs`, `compat.rs` |
| Aether store | `SCHEMA_VERSION = 28` | `crates/aether-store/src/schema.rs` |
| Aether web client | speaks protocol **18**, package `1.0.1` | `crates/aether-remote/web/core/CONTRACT.md` |
| Archon daemon protocol | `PROTOCOL_VERSION = 23`, exact match only | `crates/archon-core/src/protocol.rs` |
| Archon store | migrations V1..V18 | `crates/archon-queue/src/store/schema.rs` |
| Archon's Aether client | speaks 24, falls back to 17 | `crates/archon-aether/src/wire.rs` |

## 2. Dependency facts that drive the architecture

- **Archon has no Cargo dependency on any Aether crate.** `crates/archon-aether`
  is a hand-written client for the Aether wire protocol. It is checked against
  vendored Aether JSON schemas in `crates/archon-aether/tests/fixtures/`. At
  runtime Archon starts its own unmodified `aether` daemon ("dedicated" mode)
  or attaches to the operator's ("shared" mode). Aether knows nothing of
  Archon.
- **Each repository ships one static binary** (`aether`, `archon`) for 4 targets:
  x86_64/aarch64 musl Linux and x86_64/aarch64 macOS. The release jobs make a
  draft GitHub release with `SHA256SUMS`. `install.sh` installs into
  `~/.local/bin` without sudo. Neither build needs Node today.
- **Both web apps are embedded with `include_bytes!`** by a `build.rs` that walks
  a `web/` directory. Both are plain ES modules with no framework, no bundler
  and no runtime npm dependencies. npm is used only for tests.
- **Aether already generates typed clients.** `aether schema` produces
  `clients/protocol.schema.json` (schemars). Generators emit
  `clients/typescript/src/protocol.ts` and Python types, and
  `scripts/generate_clients.py --check` is a CI drift gate. **Archon has no schema
  and no generated types**: serde in `archon-core` is the only source, and the
  web app copies request shapes by hand (`core/daemon.js`).

## 3. Inventory to reuse

### 3.1 Web apps

| | Aether: `crates/aether-remote/web/` | Archon: `crates/archon-remote/web/` |
| --- | --- | --- |
| Stack | vanilla ES modules, JSDoc type-checked by `tsc --checkJs`, CSS variables | vanilla ES modules and CSS, no types |
| Screens | `fleet` (SVG delegation graph with pan/zoom, packing, list fallback), `attention`, `activity`, `agent/:id` (virtualized transcript with a Fenwick index, streaming tail, markdown, tool cards, search, composer), `controls/:id`, `settings`, `new`, `pair` | `pairing`, `queue`, `item` (stage timeline), `inbox`, `chat`, `overview`, `new-item`, `settings` |
| State | normalized store `core/store.js`. Applies events where `seq > lastSeq`; deltas are transient. Bounded localStorage checkpoint, cross-tab `navigator.locks` | tiny observable `core/store.js`. Refetches summaries 350 ms after any feed item |
| Reliability | `core/{transport,recovery,submissions,ledger,receipts,unconfirmed}.js`: idempotency keys, `resolve_submission`, lost-reply recovery | request queue while reconnecting, backoff, feed cursor |
| Tests | Node units; Playwright against a mock daemon (`dev/server.js`, real signatures, schema-validated); a real-daemon live suite (`scripts/live.js`) | `node --test` units; Playwright against a real archond (`make e2e`, release only) |

**Reuse:** Aether's `core/` (transport, identity, recovery, ledger, normalize,
events) is framework-free and correct; it is ported into the new core package.
Aether's `features/fleet/graph/*` (layout, packing, viewport) and
`features/conversation/virtual*` (Fenwick-indexed virtualization) carry over as
framework-agnostic modules. Archon's app has no logic worth porting beyond its
request shapes and flows, and the flows become e2e fixtures.

### 3.2 Remote listeners

| Guarantee | Aether (`crates/aether-remote`, `docs/remote.md`) | Archon (`crates/archon-remote`, `docs/remote.md`) |
| --- | --- | --- |
| Listener | loopback HTTP/1.1 `127.0.0.1:8377`, off until `aether remote enable`; `allow_remote` needed to bind non-loopback | loopback `127.0.0.1:8378`, off by default, same rule |
| TLS / publishing | `tailscale serve` via `aether remote setup`; Funnel only by explicit opt-in | `tailscale serve` via `archon remote setup`; refuses Funnel |
| Device identity | non-extractable P-256 key per browser (IndexedDB); the daemon stores only the public key | 256-bit secret in an `HttpOnly; SameSite=Strict` cookie; SHA-256 stored in `remote.json` |
| Pairing | one-time 40-bit code (120 s) plus a 3-word phrase the host confirms | one-time 40-bit code (5 min), rate-limited |
| Handshake | daemon `challenge{nonce, instance_id}`; signature binds origin + nonce + instance | `hello` exactly at protocol 23 |
| Roles | `operator`, `observer`; access classes `observe/operate/host_only/remote_admin/device_self` in one exhaustive match | **none**: every device gets the fixed allowlist in `allow.rs` |
| Host-only | `shutdown`, memory accept/reject/feedback, `memories_in_context`, run export/import, all remote admin | `shutdown`, settings writes, repos, merges, learning edits, remote admin, `operator_command` |
| Host / Origin | exact `Host`; `Origin` on `/ws` and POST; `Sec-Fetch-Site` must be `same-origin` | same checks |
| Revocation | `revoke_device` closes sessions (4403) | `archon remote revoke` closes sessions (4401) |
| Resync | `subscribe{after}`; `history_expired` triggers a full resync; `instance_id` change discards the cache | feed cursor `subscribe_feed{after}`; client-driven |
| Idempotency | 18 keyed commands, 7-day receipts, `resolve_submission`, `submission_horizon` | `enqueue.idempotency_key` only; `Tracked` requests are not on the web allowlist |
| CSP | `connect-src 'self'`, `frame-ancestors 'none'`, no inline script | same |

Known defect to fix in this release: Archon's `allow.rs` admits
`pause`/`resume` without checking `hard`, and the request runs with CLI
standing. The shipped app never sends `hard:true`, but a hand-made frame would
pass. This is task AR-R in WORKSTREAMS.md.

### 3.3 Daemon protocols and subscriptions

- **Aether:** NDJSON over a Unix socket (64 MiB line limit), or one JSON frame
  per WebSocket message. `Hello`/`Welcome` negotiate a version and `features[]`;
  `Command::introduced()` gates new commands. `seq` is global, gapless and
  scoped to one daemon instance (`Welcome.instance_id`). Read models already
  present: `list_agents`, `get_tree` (subtree usage), `list_runs`, `status.runs`
  (rate and forecast), `get_attention`, `get_transcript{agent, after}`,
  `get_events`, `providers` (headroom), `get_run_report`. Deltas
  (`text`, `thinking`, `tool_input`, `accounting`) carry **no sequence**, are
  never replayed, and are dropped when the subscriber lags.
- **Archon:** the same NDJSON framing; `ClientFrame{req, cmd}` and
  `ServerFrame{reply|feed}`. Request classes `Read/OperatorRead/Change/OperatorChange`
  come from an exhaustive `Request::class()`. The only push stream is the
  operator **feed** (`FeedItem{seq, entry}`: chat, notifications, questions,
  reports). It is not a state-change stream. There is no reverse-dependency
  request, no graph request, no item transition history, and no request for a
  steered run's frozen stage list.

### 3.4 Memory and lessons

- **Aether operational memory** (`crates/aether-memory`, `aether-store`): a
  `memories` table plus FTS5 and `memory_feedback`. Kinds are
  `fact/decision/lesson/constraint/preference`. Scopes are
  `global/repository/role/agent/repository_role/subtree`. Agents write through
  the `remember` tool and are admitted immediately by `AdmissionPolicy`. Dedup
  matches exact text only. Retrieval scores lexical overlap plus a hashed
  trigram vector, takes at most 6 memories, and caps context at `window/40`
  clamped to 400..2000 tokens. Feedback reorders by a factor of 0.75..1.25 and
  ignores `outcome` evidence. No memory evaluation exists. Seams to build on:
  the `MemoryIndex` trait, `AdmissionPolicy`, `memories_in_context`,
  `snapshot.created.layers[].memories`, and repository knowledge seeding
  (`.aether/knowledge/**`).
- **Archon lessons** (`archon-core/src/lesson.rs`,
  `archon-queue/src/store/lessons*`, `archon-daemon/src/learned/*`): sources are
  operator, orchestrator (proposed, weight at most 0.15) and distilled ($1
  errands). Dedup is by normalized text within one scope. Selection takes at
  most 6 lessons / 1,200 bytes, frozen per run, injected under
  `## Learned advice`. Confidence is a Wilson lower bound with a 30-day decay.
  Outcomes and PR merge/close observation (14 days) feed wins. The Aether
  memory-feedback bridge exists (`archon-aether/src/memory_feedback.rs`).
  No evaluation measures whether lessons help.

### 3.5 GitHub, dependencies, pipelines

- **GitHub watcher:** Archon does not poll GitHub. Aether's watcher (mention and
  label regex, collaborator and operator checks) delivers requests to the
  orchestrator agent. The orchestrator calls `enqueue` with
  `origin{repository, number}`, stored only in the item JSON, so there is no
  issue-to-item index. Replies carry `REPLY_MARKER` to prevent loops.
- **PR links:** `run_pull_requests(run, host, repository, number, node_id)` (V15),
  `ItemSummary.pr_url`, `stage_runs.structured.pr_url`; `gh` observes merged or
  closed state for 14 days and feeds it only into outcomes.
- **Dependencies:** `QueueItem.dependencies` (`depends_on` on the wire). Cycles are
  rejected in the store. An item runs only after every dependency succeeded; a
  failed or cancelled dependency makes it `Blocked`. Dependents stack on their
  dependency's branch (`stacks`). There is a reverse index `item_deps_reverse`.
  **Initiatives** are multi-repo with `members[].after` edges and an `atomic`
  ship.
- **Pipelines:** `pipelines/*/pipeline.toml`, a linear chain with fix-loop
  back-edges (`on_fail.goto`) and fan-out slots. Each run is frozen with steer
  edits (`executor/frozen.rs`, `run_edits`). Stage records: `PipelineRun`,
  `StageRun{attempt, fan_index, state, verdict, cost_usd, aether_agent,
  aether_task}`, `OperatorGate`, `StageDelegation.agents`.
- **State:** `QueueState` is
  `queued/dispatching/running/blocked/succeeded/failed/cancelled`. Read-time
  `WaitingReasonCode` covers dependency, operator, budget, pause, quota and
  load. Persistence is SQLite `archon.db` (WAL, indexed columns plus a JSON
  `data` column).

### 3.6 Tests, packaging and release

| | Aether | Archon |
| --- | --- | --- |
| Gate | `make ci` = `scripts/ci.sh` (lint, test, binary) with a host-wide `flock` slot | `make ci` = fmt-check, clippy, rustdoc, tests, with the same slot |
| CI | `ci.yml` (PRs into `next` run lint only), `ci-macos.yml` (label), `upgrade.yml` (nightly store/protocol compatibility), `release.yml` | `ci.yml` (lint; nextest on pushes to `next` and PRs into `main`; e2e only on PRs into `main`), `release.yml` |
| e2e | `scripts/e2e_mock.sh` (real binary + mock OpenAI), web live suite | `make e2e` (Playwright vs real archond), release only |
| Bench | `make bench-release` (frozen task sets, USD budget), release only | `bench/` (`archon-bench`), release only |
| Release | tag `v*.*.*` builds the 4-target matrix, attests provenance, and creates a draft release | same |

Operator rules that bind this release: run `make bench-release` and e2e only
for releases; run one heavy job on the host at a time; commit with the
configured identity; and never merge, tag or publish automatically.
