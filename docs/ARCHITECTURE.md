# Archon architecture (v1)

Archon is a hierarchical orchestration layer on top of the
[Aether](https://github.com/mengsig/aether) daemon. The operator talks to one
**orchestrator** agent. The orchestrator plans, queues and delegates work across
many repositories through **pipelines**. The Archon daemon runs those pipelines
by spawning ordinary Aether agents, and it enforces caps, budgets and the model
allowlist.

This document is the design reference: how the components fit together, the
types in `archon-core` they share, and the interfaces between them.

Contents:

1. [Principles](#1-principles)
2. [Components and processes](#2-components-and-processes)
3. [Crate boundaries](#3-crate-boundaries)
4. [Aether integration](#4-aether-integration)
5. [Data flow](#5-data-flow)
6. [State machines](#6-state-machines)
7. [Scheduling, caps and budgets](#7-scheduling-caps-and-budgets)
8. [Pipelines](#8-pipelines)
9. [Configuration](#9-configuration)
10. [The Archon daemon protocol](#10-the-archon-daemon-protocol)
11. [MCP tool surface](#11-mcp-tool-surface)
12. [Operator chat and filtering](#12-operator-chat-and-filtering)
13. [Repositories and worktrees](#13-repositories-and-worktrees)
14. [Lavish plugin](#14-lavish-plugin)
15. [Benchmarks](#15-benchmarks)
16. [Failure modes and recovery](#16-failure-modes-and-recovery)
17. [Aether limitations and how Archon handles them](#17-aether-limitations-and-how-archon-handles-them)
18. [Autonomy](#18-autonomy)
19. [Learning](#19-learning)

---

## 1. Principles

- **Aether is not modified.** Archon speaks Aether's public socket protocol.
  By default it runs its own unmodified `aetherd` with its own config, store
  and socket, and it never writes the operator's Aether configuration (§4.7).
  In shared mode, `archon setup` adds one `[[mcp.servers]]` entry to it.
- **No accuracy loss.** Every worker is an ordinary Aether agent on a native
  harness (`claude-code/*`, `codex/*`, or any Aether provider). Archon never
  runs its own model loop. It only composes prompts, starts agents, and reads
  their results.
- **Quiet by default.** The operator chat shows questions, decisions, blockers,
  outcomes and direct replies. Everything else stays in Aether's TUI and
  `archon log`.
- **The orchestrator decides and the daemon enforces.** The orchestrator
  picks the pipeline and its review level, and the model and effort of the
  stages it has a reason to choose for; the daemon fills the rest with
  defaults. The daemon validates every pick against the model policy, caps
  and budgets, and refuses anything outside them with an error that names
  the allowed values.
- **Durable first.** SQLite is authoritative. Every Aether call that creates
  work carries an idempotency key derived from an Archon id, so a crash at any
  point can be retried without duplicating agents.

## 2. Components and processes

```
 operator ──archon chat──▶ ┌──────────────────────────── archond ─────────────────────────────┐
                           │ socket server (archon protocol, NDJSON)                          │
 aether TUI (full detail)  │ feed (quiet chat) · dispatcher · pipeline engine · supervisor    │
        ▲                  │ aether link (1 connection, 2 subscriptions) · worktrees · lavish │
        │                  │ SQLite store (archon.db)                                         │
        │                  └───────────────▲──────────────────────────────┬───────────────────┘
        │                    archon protocol│                              │ aether protocol v17
        │                                  │                              ▼
        │          ┌──── archon mcp (stdio) ◀── spawned by ──┐   ┌───────── aetherd ─────────┐
        └──────────┤                                         └───┤ orchestrator agent (root) │
                   │                                             │ run node (root, passive)  │
                   │                                             │   └─ stage agents         │
                   │                                             │        └─ their delegates │
                   └─────────────────────────────────────────────┴───────────────────────────┘
```

| Process | What it is | Lifetime |
| --- | --- | --- |
| `archond` (`archon daemon run`) | The Archon daemon: store, dispatcher, pipeline engine, Aether link, orchestrator supervisor, feed, lavish plugin. | Long-running; autostarted by any `archon` client. |
| `archon` | The CLI, a client of `archond`. With no subcommand it opens the terminal UI (`archon tui`, §4.7); `archon chat` is the quiet operator chat (§12). | Per command. |
| `archon mcp` | Stdio MCP server. Aether spawns it as the `archon` MCP server, and it proxies each tool call to `archond`. | Owned by `aetherd`. |
| `aetherd` (Archon's own) | In dedicated mode (the default), an unmodified `aether daemon` with its config, store and socket under Archon's directories (§4.7). | Started by any `archon` command that needs `archond`; stopped by `archon daemon stop --aether`. |
| orchestrator | One long-lived root Aether agent (`role = "archon-orchestrator"`). Uses native tools plus `mcp__archon__*`. | Created once and supervised by `archond`. |
| run node | One passive root Aether agent per pipeline run (`role = "archon-run:<pipeline>"`), never started. Its children are the run's stage agents, so Aether's tree shows one subtree per item. | Created with the run's first stage agent; cancelled when the run finishes. |
| stage agents | One Aether agent per stage attempt and fan-out slot, a child of its run node. | Created and settled per stage. |

## 3. Crate boundaries

```
archon-core ◀── archon-aether ◀──┐
     ▲  ▲  ◀── archon-queue  ◀───┤
     │  └───── archon-pipeline ◀─┼── archon-daemon ◀── archon (bin)
     │         archon-lavish ◀───┘                       │
     ├──────── archon-client ◀── archon-mcp ◀────────────┤
     │                                                   │
     └──────── archon-bench (bench/) ◀── archon-client, archon-aether
```

| Crate | Owns | Must not |
| --- | --- | --- |
| `archon-core` | Ids (Archon's and Aether's), domain records (repo, queue item, run, stage run, feed), `Config` schema and loading, `ModelPolicy`, the Archon daemon protocol (`protocol`), the `AgentBackend` port trait (`ports`), pipeline catalog vocabulary (`catalog`), and the MCP tool names (`mcp_tools`). | Do I/O beyond reading config files. |
| `archon-aether` | Rust client for Aether protocol v17 (falls back to 16, 15 and 14): transport, handshake, request multiplexing, subscriptions, typed subset of commands and events (`wire`, `events`), the reconnecting `AetherLink`, and `AetherBackend: AgentBackend`. | Know about queues or pipelines. |
| `archon-queue` | `Store` (SQLite, migrations): repos, items, dependencies, runs, stage runs, feed, questions, spend ledger, pause state, kv cursors. Pure scheduler `plan_dispatch`. | Talk to Aether. |
| `archon-pipeline` | Pipeline discovery (built-in, user, repo), TOML parse and validation, prompt templating, and the `RunDriver` state machine (gates, loops, fan-out). | Do I/O except reading pipeline files; spawn anything. |
| `archon-client` | Async client for the Archon daemon socket (`connect`, `connect_or_start`, `request`, `subscribe_feed`), and the NDJSON framing shared with the server. | Contain daemon logic. |
| `archon-mcp` | MCP stdio server (JSON-RPC 2.0): tool schemas, tool-name to `Request` mapping, result rendering. | Hold state; everything goes to `archond`. |
| `archon-lavish` | `Lavish` plugin: detect `lavish-axi`, open/poll/end sessions, parse feedback, degrade when absent. | Talk to Aether or the store. |
| `archon-daemon` | `archond`: socket server, request handlers, dispatcher loop, event pump, orchestrator supervisor, feed builder, worktree manager, engine action executor, orchestrator prompt. | Parse CLI args. |
| `archon` (bin) | clap CLI, `archon chat` renderer, `archon setup` (seeding Archon's own Aether, or MCP registration in shared mode), the dedicated `aetherd` lifecycle (`instance`), `archon daemon`, `archon tui` and `archon aether` subcommands, `archon mcp` entry. | Contain daemon logic. |
| `archon-bench` (`bench/`) | Benchmark runner, arms, graders, report. | Be a dependency of anything. |

## 4. Aether integration

Everything below was checked against Aether's source (`aether-protocol`,
`aether-core`, `clients/protocol.schema.json`, protocol version **15**; the
daemon also serves 14).

### 4.1 Transport and framing

- Unix socket: `$AETHER_SOCKET`, else `$XDG_RUNTIME_DIR/aether/aetherd.sock`,
  else `$AETHER_DATA_DIR|~/.local/share/aether/aetherd.sock`. Optional TCP:
  `[aether] address` + `token_env` (the token must have at least 32
  characters). Over TCP the daemon closes idle connections after
  `Welcome.idle_timeout_secs`, so the client pings every `idle/3` seconds.
- Newline-delimited JSON. Frames are capped at 64 MiB.
- Client frame: `{"req": <u64 starting at 1>, "cmd": "<snake_case>", ...fields}`.
- Server frames:
  - `{"frame":"reply","id":N,"result":{"Ok":{"reply":"<kind>","data":...}}}`
    or `{"frame":"reply","id":N,"result":{"Err":{"code":"...","message":"..."}}}`.
    `id: 0` is the daemon explaining why it is about to close the connection.
    Keep it as the disconnect reason.
  - `{"frame":"event","sub":S,"event":Event}`, where `Event = {id, seq, at,
    actor, agent_id?, task_id?, parent_agent_id?, run?, causation?,
    kind:{"type":"task.completed", ...}}`.
  - `{"frame":"delta",...}` is never requested by Archon (`deltas: false`).
- Error codes: `incompatible`, `unauthorized`, `not_found`,
  `invalid_transition`, `invalid_argument`, `unavailable`, `internal`.

### 4.2 Handshake

```json
{"req":1,"cmd":"hello","protocol_version":17,"client_name":"archon",
 "client_version":"1.1.0","features":[]}
```

The reply is `Welcome {protocol_version, daemon_version, instance_id,
schema_version, pid, features, daemon_protocol_version?, min_protocol_version?,
idle_timeout_secs?}`. If Aether refuses the handshake with `incompatible`
and the newest protocol it serves is 16, 15 or 14, Archon reconnects and
says hello at that one: an aetherd not yet upgraded keeps working. What its
protocol cannot store, Archon refuses to save, before anything is sent:
GitHub operator logins below 17 (`operator_users`), and a GitHub watcher
allowlist at 14, which would widen who may dispatch. An aetherd that
answers a watcher read with `incompatible` because operator logins are set
is reported as that, never as a reply that failed to decode. Any other
refusal is reported as "Archon speaks Aether protocol 14..17; the daemon
serves {min}..{max}". A change to `instance_id` between connections means
Aether restarted, which triggers reconciliation (§16).

### 4.3 Commands Archon uses

| Purpose | Command | Notes |
| --- | --- | --- |
| Liveness | `ping`, `status` | `status` also feeds `archon doctor`. |
| Model discovery | `list_models` | Checks the allowlist against models Aether can actually run. A listed model that Aether does not know produces a warning, not a refusal. |
| MCP health | `mcp_servers {refresh:false, server:"archon"}`, then `{refresh:true}` when it failed | On every connect and every minute, so Aether reconnects `archon mcp` if it died (§16). `archon status` and `archon doctor` read it too. |
| Create orchestrator | `create_agent` | See §4.4. |
| Create run node | `create_agent {start:false}` | See §4.5. `idempotency_key` = `<run id>:node:<generation>`. |
| Create stage agent | `create_agent` | See §4.5. `idempotency_key` = stage run id; `parent` = the run node. |
| Operator → orchestrator | `send_message {to, content, idempotency_key}` | Key = feed item id. |
| Team roster | `send_message {to, content, idempotency_key}` | Key = `roster:<stage run>:v<version>`; §13.1. |
| Wake orchestrator with Archon news | `trigger {agent, source:"archon", payload, idempotency_key}` | Payload schema in §5.4. |
| Answer an `ask_user` | `reply_to_question {to, question_seq, content}` | Used when the question came from Aether's `ask_user`. |
| Read results | `get_task {id}` | `TaskStatus::{Completed{result:{summary,artifacts,structured}}, Failed{error,structured}, Cancelled{reason}}`. |
| Inspect agents | `get_agent {id}`, `list_agents {query}` | Supervisor and reconciliation. |
| Stop work | `cancel {agent, reason}` | Cancels the agent and its live descendants. |
| Revive orchestrator | `wake {agent}`, then `restart {agent}` as fallback | §16. |
| Tune a live agent | `update_agent {id, patch}` | Deadline extension, cost budget top-up. |
| GitHub watcher | `get_github_watcher`, `configure_github_watcher {config, expected_revision}` | Dedicated mode only: the operator's `archon github-watcher` edits, and repointing Archon's `fixed` repositories at the orchestrator (`docs/github-watcher.md`). Every write sends back the `operator_users` it read; only `archon github-watcher operators` changes them. |
| Pause fleet | `pause {agent, subtree}`, `resume` | `archon pause --hard` also pauses running stage agents in Aether. |
| Events | `subscribe {after, query, deltas:false}`, `get_events` | Two subscriptions, §4.6. |

Archon never sends `shutdown`, never edits approvals, never touches memory
or schedules, and never touches the GitHub watcher of an Aether it shares.

### 4.4 The orchestrator agent

```jsonc
{"cmd":"create_agent",
 "role":"archon-orchestrator", "name":"archon",
 "model":{"provider":"claude-code","model":"claude-opus-5-5"},  // [orchestrator].model; must pass ModelPolicy
 "scope":{"workspace_root":"<data>/orchestrator",
          "network":"<status.network_default>",        // see below
          "labels":{"archon":"orchestrator"}},
 "config":{"instructions":"<built-in orchestrator prompt + [orchestrator].instructions_file>",
           "effort":"high",
           "long_lived":true,
           "tools":{"mode":"except","names":["integrate_workspace"]}},
 "idempotency_key":"archon-orchestrator-<generation>",
 "start":true}
```

- It is a **root** agent. Archon stores its id in `kv.orchestrator_agent`.
  Stage work does not hang under it (§4.5): Aether's `cancel` cascades to
  live descendants, and Archon cancels what a replaced orchestrator left
  running (§16), so replacing the orchestrator would cancel every in-flight
  stage; a child's scope must
  sit inside its parent's, and the orchestrator's holds no worktree; and
  a child may not gate less than its parent, while stage agents gate nothing.
- An explicit `scope` gets `network: denied` unless it says otherwise. Aether
  applies its `[network] default` only to roots created without a scope, so
  Archon reads `status.network_default` and states it (§17).
- No `approval` is sent, so the operator's `[approval] default` applies. The
  orchestrator is attended: the operator can answer its approvals in Aether.
  With `tools = "yolo"` in the global `[autonomy] default`, Archon sends
  `"approval":{"require_for":[]}` instead, from the next generation on
  (`archon orchestrator restart` applies it at once; §18.4).
- It keeps Bash, file tools, web tools and Aether's `ask_user`, `wait_for`,
  `remember`, `send_message`, `list_agents`, `read_task`, `read_artifact` and
  `delegate`. It loses `integrate_workspace` (§18.4), and `delegate` too
  while Aether does not name the agents that call Archon's tools (§11).
- Its workspace holds `repos/<name>`, a mirror of each registered repository
  (§13.3), and `scratch/` for work in no repository. It reads code in the
  mirrors. Its native tools are not confined to the workspace (an Aether
  limitation, §17).
- `archon-daemon/prompts/orchestrator.md` is the built-in prompt, held to
  12 KiB by a test. It gives the order in which to decide how a request is
  done (answer, read the mirror, delegate, enqueue the cheapest pipeline that
  fits), the review level and model plan of an item, what a helper is given,
  the two operator views, which news wakes it (§5.4) and the GitHub policy.
  What one tool does is said once, in that tool's description (§11), and the
  prompt does not repeat it. Archon appends `## This installation` to it:
  the orchestrator's workspace, where its mirrors and scratch directory are,
  and the user pipelines directory, resolved for this machine.
- Every Archon tool is the orchestrator's, and tests hold that the prompt
  names each one. After the paths Archon appends `## Learned observations`,
  the lessons chosen for this generation of the orchestrator (§19): quoted,
  under their own heading, and said there to be advice that grants nothing.

**What the orchestrator does without an item.** It answers what it knows or
can look up, reads code in a mirror, and delegates what would flood its
context or splits into parts: research on the web or in documents, a wide
read of a subsystem, a comparison, a draft, a second look at a result. A
question about a repository's code is answered in the turn that asks it,
with no item, no worktree and no ship stage. `ask` is still a pipeline, for
a question worth a recorded run and for a repository whose mirror is
withheld (§13.3), but nothing requires it. Every change to a repository is
an item (`enqueue`), so that Archon's worktrees, ship modes, gates, caps and
budgets apply. The prompt says so, and the daemon stands behind it:

- *A helper works in a mirror or in scratch, never in a repository.* Aether
  accepts a child's `workspace_root` only under its parent's, and the
  mirrors are under the orchestrator's, so a helper is given one mirror:
  shared, to read it, or `workspace_mode: "isolated"`, which Aether makes a
  git worktree of that mirror on a branch of the mirror's own, to build or
  try something. Neither reaches the operator's checkout (§13.3). The
  watchdog (§7.4) reads each helper's scope from Aether and cancels one whose
  `workspace_root` or `allowed_paths` lead into a registered repository or
  the worktrees root, resolving symlinks, since Aether compares scope paths
  as written (`Scope::is_narrowing_of`). What stays inside the orchestrator's
  own workspace, its mirrors included, passes. The check covers where a
  helper was placed, not what a native engine's tools can open beyond it
  (§17).
- *No integration.* `integrate_workspace`, which copies an isolated child's
  worktree back, stays hidden: the orchestrator ships nothing, and what a
  helper commits in a mirror stays in the mirror.
- *No delegation for an external request.* The prompt forbids `delegate` for
  a GitHub request without operator standing; such a request is answered
  directly, from that repository's mirror when it takes code. Only the
  prompt holds this: the daemon
  cannot tell which request a helper serves. It reads a trigger, and the
  turn that takes it, on the orchestrator's event stream, and a helper's
  creation on the fleet's, which has its own cursor; and one turn may take
  an external request together with an operator message or a helper's
  result. The rule sits in the prompt's
  delegation section, and its `## GitHub requests` section is unchanged, so
  an orchestrator started by an earlier release, which has no `delegate`,
  still passes the GitHub policy check (`has_github_policy`).

**What a delegated child is.** It inherits the orchestrator's `config`:
its tool exposure and approval policy, though not its instructions. Aether
gives a child only the `instructions` its parent passes to `delegate`, or
none, so no rule of the prompt reaches a child by itself: the prompt has
the orchestrator state in each child's `instructions` that it commits and
pushes nothing, stays in the workspace given, overrides no git identity,
asks the operator nothing, and passes the same rules to any agent it
starts. So:

- It sees Archon's MCP tools. `archond` lets a mutating one through only
  for the current orchestrator, by the caller Aether names on the call
  (§11), so a child reads the queue but cannot change it.
- The stage-agent watchdog adopts it (§7.4). A helper is held to the global
  `[models]` policy narrowed by the policy of each repository whose mirror
  its scope reads: one given the mirror of `api` answers to `api`'s policy
  and to no other repository's, and one given `scratch/` to the global
  policy alone. A model or an effort that policy refuses gets the helper
  cancelled, and the orchestrator hears `policy_violation`. Its spend counts
  toward the daily and total budgets.
- It may delegate once more. An agent a third delegation below the
  orchestrator is cancelled for its depth (§7.4), whatever its model. That
  is `[limits] max_delegation_depth = 2`, which the "This installation"
  section of its prompt states.
- It is outside `max_parallel_agents` and `max_stage_agents_per_item`, which
  count stage agents (§7.1). The prompt holds the orchestrator to the
  parallel-agent cap, counting its own children with Aether's `list_agents`,
  and to no children while a budget holds dispatch; the daemon does not
  enforce either.
- Its result reaches the operator through the orchestrator. A turn that
  ends waiting for a child leaves the operator's message open, so the reply
  the orchestrator gives when the child reports is the one `archon chat`
  shows (§12).
- It is the orchestrator's child in Aether, so it ends when the orchestrator
  is replaced. Aether's `cancel` takes an agent's live descendants along. A
  failed orchestrator leaves them running, so when a new agent takes over
  from one, Archon cancels the old agent's subtree, whose results would
  otherwise reach nobody. Each retired generation is recorded before the new
  one starts and swept in the background until Aether confirms, across
  restarts of `archond` and along Aether's own restart chain (§16).

### 4.5 Stage agents and run nodes

Aether's tree mirrors Archon's structure one level down from the
orchestrator: every pipeline run has a **run node**, a passive Aether agent
that its stage agents are children of, and anything a stage agent delegates
sits under that stage agent. The orchestrator stays a separate root (§4.4).
In `archon tui` (Aether's TUI) an item therefore reads as

```
Archon                                  archon-orchestrator
add a greeting                          archon-run:default-pr-review
  #q69g5fav implement · add a greeting            archon-stage:default-pr-review/implement
  #q69g5fav review 1/2 · add a greeting           archon-stage:default-pr-review/review
  #q69g5fav review 2/2 · add a greeting           archon-stage:default-pr-review/review
  #q69g5fav ship · add a greeting                 archon-stage:default-pr-review/ship
```

`archon aether agents` passes through to Aether's flat table, which shows
the readable names in its truncated name column; its `--json` output
carries each agent's `parent`.

**The run node** is created the first time a stage of the run is issued:

```jsonc
{"cmd":"create_agent",
 "role":"archon-run:<pipeline>", "name":"<item title, one line, ≤ 40 chars>",
 "model":{"provider":"…","model":"…"},                 // the first stage's model; children state their own
 "scope":{"workspace_root":"<worktree>", "repository":"<repo.name>",
          "network":"<status.network_default, read once for this generation and stored>",
          "labels":{"archon.item":"qi_…","archon.run":"prun_…","archon.pipeline":"<pipeline>"}},
 "approval":{"require_for":[]},
 "config":{"leaf":false, "long_lived":false},          // no instructions, effort, budget or deadline
 "run":"<the run's Aether run id, if one exists already>",
 "idempotency_key":"<pipeline run id>:node:<generation>",
 "start":false}
```

- It has no `objective` and `start: false`, so Aether never gives it a task
  or a turn: it is `created` for its whole life, costs nothing, and is not
  woken by its children settling (Aether notifies a parent only of a child's
  *failure*, and a `created` agent is never scheduled).
- Its scope is the run's worktree (for a team run, the item directory that
  holds the run worktree and every role's worktree, §13.1) and its approval
  gates nothing, so every
  stage agent (same worktree, same repository, the same or a narrower
  network, `require_for: []`) narrows it, which Aether requires of a child.
  The node's network is Aether's `status.network_default` when the
  generation's request is built, stored with it. `[network] default` may
  change while a run is under way (an Aether restart with a new config), so
  each stage agent states the narrower of its node's network and the
  default read when that stage is bound (below): never wider than its
  parent, which Aether would refuse, and never wider than what the operator
  allows now. A node created while the default was `allowed` therefore
  takes `denied` children once the operator tightens it.
- Created without `run`, the node starts the run's Aether run, which its
  stage agents inherit as its children. Archon records that run from the
  node's create reply (`agent.run`) before any stage is bound to the node,
  so the fleet pump knows which Aether run a spawning stage's agent joins
  (§4.6). Later stage agents, and a replacement node, attach to it with
  `run`, so Aether's TUI shows one run per pipeline run.
- Its `config.tools` hides every Archon tool. It never takes a turn, but an
  operator can `restart` a retired (cancelled) node in Aether; the agent
  that results is an ordinary one that cannot act for the orchestrator, and
  it is not Archon's node any more.
- `kv.run_node:<run id>` holds `{agent, generation, request}` while the run
  has a node, and `kv.run_node_agent:<agent id>` names the run of every
  node agent Archon learned of, kept after retirement, so the fleet pump
  skips a node's own events at once (§4.6). The store pruning pass (§16)
  moves an index entry to a minimal `kv.message_run_node:<agent id>` routing
  tombstone once its run finished more than the 7-day retention ago and
  its node was retired. The tombstone is retained for one additional 7-day window so late inbox
  messages still reach the orchestrator, then deleted. Before a new stage agent is bound to it (below) the daemon
  reads it and asks `get_agent` whether the node is still live. A node
  Aether lost (wiped store, cancelled in its TUI) is replaced under the next
  generation's key; the run goes on.
- Aether's idempotency fingerprints the whole `create_agent` frame under
  its key, so a generation's request is decided once: it is persisted
  (`request`, with the model of the stage that needed the node and the
  network default read just before) under the store lock before the first
  `create_agent`, and every retry and every concurrent stage of the run
  sends that stored request. The request stays with the recorded `agent`,
  since later stages narrow its network. A create Aether refuses
  (`invalid_argument`: a record it cannot replay; `not_found`: work it
  created and lost) moves the node to the next generation once; a second
  refusal fails the stage. `unavailable` leaves the request pending and
  re-issues the stage under its key on the next pass.
- **Retirement.** When the run finishes and none of its stage runs is
  live any more, the node is cancelled and its `kv` entry deleted. Aether
  settles a task in the middle of the agent's turn (`complete_task`), and
  the agent completes only when the turn ends, seconds later for an engine
  agent; its cancel cascades through the node's live subtree. So before
  cancelling, the daemon asks `get_agent` for the stage agents that settled
  within the last 60 s, most recently settled first, and waits while one
  is still live, so the last stage ends as `completed` rather than
  `cancelled` and its final turn is not cut short. An agent more than 60 s
  past its own settlement is not asked about: its grace is over and the
  node is cancelled regardless, so a long run's history (up to 32 stages,
  16 agents each) is never queried, and the usual case (the last stage
  still ending its turn) costs one `get_agent`. Retirement is derived from the
  store, not from memory: `stop_run` asks for it at once, and every
  dispatch sweep ranges over `kv.run_node:*` (a primary-key range, so it
  costs what is still to retire, not the store's history) for finished runs
  with no live stage and asks again, so a wait, a cancel that went
  unanswered, an error, or a crash between the run finishing and the cancel
  is repaired on the next pass or after a restart. A generation whose
  create was never answered is replayed under its key first, so a node
  Aether created behind a lost reply is found and cancelled rather than
  leaked.

**Stage agents** are created under the node:

```jsonc
{"cmd":"create_agent",
 "role":"archon-stage:<pipeline>/<stage>",
 "name":"<stage>[ <fan>/<fan_out>][ (attempt <n>[, retry <r>])] · <item title>",
 "objective":"<rendered stage prompt>",
 "model":{"provider":"…","model":"…"},                 // ModelChoice from the item's ModelPlan
 "scope":{"workspace_root":"<worktree>", "repository":"<repo.name>",
          "network":"<the narrower of the run node's and status.network_default>",  // read and stored when it is bound
          "labels":{"archon.item":"qi_…","archon.run":"prun_…","archon.stage":"<id>",
                    "archon.stage_run":"stg_…","archon.attempt":"<n>","archon.fan":"<0-based slot>"}},
 "parent":"<run node id>",
 "approval":{"require_for":[]},                       // headless: gate nothing
 "config":{"instructions":"<stage contract, §8.6>",
           "effort":"<effort>",
           "leaf":true,                                // false when stage.allow_delegation or tools = yolo
           "long_lived":false,                         // true for the agent kept through a fix loop (§6.7)
           "cost_budget":<remaining item budget, a team slot's share of it (§13.1), or omitted>,
           "deadline":"<now + stage.timeout_minutes>",
           "tools":{"mode":"except","names":["mcp__archon__enqueue", "…every archon tool…",  // yolo: only the mutating ones
                                             "create_schedule", "update_schedule", "list_schedules"]}},  // §18.4
 "run":"<the run's Aether run id (its node's), if known when reserved>",
 "idempotency_key":"<stage run id>",
 "start":true}
```

- **Names are for people.** Aether's TUI and `aether agents` truncate names,
  so the part that tells agents apart comes first and the item's title last:
  `#q69g5fav implement · add a greeting`, `#q69g5fav review 2/3 · add a greeting`,
  `#q69g5fav implement (attempt 2) · add a greeting`,
  `#q69g5fav ship (retry 2) · add a greeting`.
  The fan slot appears only when the stage fans out, the attempt only from
  the second one, the retry only above one for an infrastructure replacement (§6.3).
  The short item id is the same lower-case eight-character ULID suffix used
  in worktree branch names; labels remain authoritative for identity.
  The title is whitespace-collapsed and clipped to 40 characters, and the
  stage id (valid up to 64) to 24, each independently, so a long id cannot
  push the title out of a truncated column. Nothing
  parses names: the `archon.*` labels and the role carry the exact identity,
  and the store maps agent ids to stage runs.
- The stored `SpawnRequest` (§6.3) is reserved with `parent: pending`. The
  first time the spawn is issued, the daemon reads `status.network_default`,
  resolves the run's node, writes the request back with that parent, the
  narrower of the node's network and that default, and `parent: bound`,
  and only then sends it, so every re-issue under the key repeats the frame
  Aether fingerprints: a default changed after binding never changes a
  replay. A record from before run nodes
  existed has no binding field and is therefore `bound` to no parent and
  no network: it is replayed exactly as stored, with
  `status.network_default` at send time as that daemon sent it, and the
  root agent Aether may already hold for it is found rather than refused.
  A `pending` spawn of a finished run is settled `cancelled` without
  reaching Aether: nothing was ever sent for it.
- A stage re-issued under its key after its bound node was lost is refused
  by Aether (a dead parent) and fails; the driver treats that like any
  failed stage. This needs the node to vanish between binding and a
  successful create, and the failure is visible, so the daemon does not
  special-case it.
- The reply is `Reply::Agent(Agent)`. Archon persists `agent.id`,
  `agent.current_task` (the task created from `objective`) and the Aether run
  id (from the `agent.created` event or `get_task.run`).
- Stage agents are headless, so `approval` gates nothing. Under the
  operator's `approval = "ask"` a gated tool call would otherwise wait for
  nobody until the stage deadline.
- Isolation: Archon creates its own git worktree (§13) and passes it as
  `workspace_root`. Aether's isolated-worktree mode exists only in the
  agent-side `delegate` tool, not in `create_agent`.

### 4.6 Event subscriptions

`archond` holds **one** Aether connection with two subscriptions. It persists
a cursor per subscription in `kv` after handling each event. Delivery is
at-least-once, so every handler is idempotent.

| Subscription | Query | Handles |
| --- | --- | --- |
| `orchestrator` | `{agent: <orchestrator>, types:["agent.message","agent.turn_completed","agent.waiting","agent.started","question.asked","agent.failed","agent.cancelled","agent.completed","agent.restarted","agent.usage_updated","inbox.delivered","inbox.consumed","compaction.completed"]}` | Chat replies (§12), questions, supervisor, adopting an Aether restart (§16), orchestrator spend, restating the directives after a compaction (§18.5). |
| `fleet` | `{types:["agent.created","task.completed","task.failed","task.cancelled","agent.failed","agent.cancelled","agent.completed","agent.usage_updated","question.asked","inbox.delivered"]}` | Stage completion, spend ledger, stage questions, model-policy watchdog, run-node inbox forwarding and authenticated relay provenance. An agent's end (`agent.completed`, `agent.failed`, `agent.cancelled`) also wakes the dispatcher, which stops what a settled stage agent delegated (§6.3). |

The `fleet` subscription also forwards message inbox entries addressed to
run nodes (§5.5). `agent.created` events whose `parent_agent_id` is
a known Archon agent (the orchestrator, a stage agent or a watched child) are
checked against `ModelPolicy`, and an offending agent is cancelled (§7.4).
The event's `kind` carries the model and effort Aether admitted. A stage
agent's own `agent.created` names its run node as parent; Archon checked
that model when it spawned the stage, so the watchdog only records the agent
as its run's item's (`kv.link_stage_agent_<agent>`, pruned with the item, §16). Aether emits that event
before any of the stage's own children exist, so a child delegated before
the spawn reply is recorded still has a known parent, and so do its
children at every depth.

A watched child is owned before it is checked: `kv.link_child_<agent>`
attributes its spend to the item, and `kv.link_unchecked_<agent>` marks the
verdict pending until the check ends, so an event retried after a lost
connection is checked again. A child Aether no longer knows is checked by
model alone. An effort Archon cannot parse is refused when `max_effort` is
set, since it cannot be compared with the cap.

A settling event (`task.*`, `agent.failed`, `agent.cancelled`) can arrive
before the spawn reply that tells Archon the agent is a stage's. The stream
is handled in order, so such an event waits (up to 5 s) only when it could
be one. The wait is not a poll: the executor signals each spawn whose
outcome it recorded (`DaemonContext::spawn_recorded`) and the waiting event
looks again at once, with one more look a second as a net for a stage that
stops spawning any other way. An event could be a spawning stage's when its
agent is neither a stage agent, a run node
(`kv.run_node_agent`, which covers every retired node's `agent.cancelled`
for the 7-day retention of §16)
nor a watched child, and some `spawning` stage whose request may have
reached Aether (`bound`) joins the event's Aether `run`, or that run is not
known. Every other event is handled at once, so one run's spawn never
delays another's settles, spend or watchdog checks. An event the wait
misses is still settled: the executor asks `get_task` right after recording
a new stage agent.

On (re)connect Archon subscribes with `after = cursor`. If a cursor is missing
(first start), it subscribes with `after = None` (live only) and reconciles
from `get_task` (§16).

### 4.7 Archon's own Aether, and the `archon` MCP server

Aether reads its `[[mcp.servers]]` only at daemon start, and every agent of
a daemon sees every server's tools. If Archon registered its server in the
operator's own Aether, every plain Aether agent would see Archon's tools,
and registering would mean restarting the operator's daemon. So by default
Archon runs its **own** Aether instance: `[aether] mode = "dedicated"`.

| Variable | Dedicated instance |
| --- | --- |
| `AETHER_CONFIG_DIR` | `<archon config>/aether` (`~/.config/archon/aether`) |
| `AETHER_DATA_DIR` | `<archon data>/aether` (`~/.local/share/archon/aether`) |
| `AETHER_SOCKET` | `aetherd.sock` beside archond's socket: `$XDG_RUNTIME_DIR/archon/aetherd.sock`, else `<archon data>/aetherd.sock`. Moving archond's socket with `ARCHON_SOCKET` moves this one too, so two Archon setups never share an instance. |

Every `aether` process of the instance gets these three variables, plus
`ARCHON_CONFIG_DIR`, `ARCHON_DATA_DIR` and `ARCHON_SOCKET`, so the
`archon mcp` it spawns reaches the right `archond`. The operator's own
`aetherd` is never started, stopped or contacted in dedicated mode.

**Setup.** `archon setup` creates the instance's config directory (mode
0700) and never writes to the operator's `$AETHER_CONFIG_DIR|~/.config/aether`.

| File | Treatment | Why |
| --- | --- | --- |
| `config.toml` | Copied from the operator's, then changed: `[listen]` and `[serve]` are dropped, any `archon` MCP server is dropped, and Archon's server is added. It is parsed and validated before an atomic write; a config that does not parse is refused and nothing is written. | It must differ (the MCP server). `[listen]` would fight over the operator's TCP port, and `[serve]` webhooks address the operator's agents. Engines, models, routing, network, approval and scheduler settings carry over. So does a `[remote]` section, in Aether builds that have one: that listener is off until it is enabled on the instance, which then needs an address of its own ([guide](guide.md#using-archon-from-aethers-remote-app)). |
| `providers.toml` | Symlink. | Aether only reads it, so both instances share one set of provider settings. |
| `ui.toml`, `themes/`, `voice/` | Symlinks. | TUI preferences, optional custom themes and installed voice backends. The TUI saves `ui.toml` by rename, so a change made in `archon tui` replaces the link with the instance's own copy, which is harmless. Aether's `/voice setup` resolves the instance directory and creates each install in a new directory under `voice/`, through the link, so both instances use the same installs. |
| `voice.toml` | Copied once, as the instance's own regular file (mode 0600, staged in a private temporary directory beside it and published without replacing anything). Never a link. | Aether's `/voice setup` refuses a `voice.toml` that is a link or a directory before it writes anything. The copy is correct because the file holds absolute paths. A later change to the operator's file does not reach the instance, and `/voice setup` in `archon tui` writes the instance's file, not the operator's. |
| `credentials.json` | **Not shared.** | Aether writes it by atomic rename, locked by a `credentials.json.lock` beside it, and it refuses a symlinked lock. A symlink would fork on the first token refresh, and with rotating refresh tokens one side would be logged out. The `claude-code` and `codex` engines use their CLIs' own login, which both instances share already. For a stored Aether login, run `archon aether login <provider>`. `archon doctor` warns when the operator has credentials that the instance lacks. |
| lock files, backups | Ignored. | They belong to the operator's instance. |

`archon setup` is idempotent. When the instance's config exists, it only
appends the `archon` server if that is missing (the same parse-validate-append
as shared mode) and repairs the links. It never replaces a real file with a
link. `archon setup --resync` re-seeds `config.toml` from the operator's
current config and keeps the instance's existing `archon` server entry. The
seeded file starts with a comment that names its source.

**`voice.toml`.** Earlier versions linked the instance's `voice.toml` to the
operator's, and `/voice setup` in `archon tui` then failed with "voice.toml
must be a regular file, not a link or directory". `archon setup` (with or
without `--resync`) and every `archon tui` start heal this: a link to the
operator's file is replaced by a copy of its bytes, and a dangling link is
removed so `/voice setup` can create the file. An instance `voice.toml` that
is already a regular file is never replaced, also not by `--resync`: it may
hold the result of this instance's own `/voice setup`. A link to any other
file, or a directory at that path, is left alone and reported with the path
and the fix, by `archon setup`, `archon doctor` and (on stderr) `archon tui`,
which still starts. Only `archon setup` copies the operator's file where the
instance has none; `archon tui` never creates one. To take the operator's
file again, delete the instance's `voice.toml` and run `archon setup`.

No step replaces something it did not inspect. A change is made only while
holding Aether's own `.voice-setup.lock` in the instance directory, which its
installers hold from before they read `voice.toml` until they have saved it;
while a `/voice setup` runs, nothing is changed and this is reported. Under
the lock the path is inspected again, a link is moved aside in one step and
put back if it is no longer the link that was inspected, and the copy is
published by hard link, which fails where a file already exists. A file that
appears at any point, from an editor or another process, therefore wins.

**Lifecycle.** Every command that needs `archond` (`chat`, `say`, `status`,
`daemon start`, …) first makes sure the instance answers on its socket. If
it does not, the command takes `<archon data>/aetherd-start.lock` (an
advisory `flock`), checks again, and runs `aether daemon` with the instance
environment in the instance's data directory. That command returns once
the detached daemon listens. `archond` itself never starts Aether. In
dedicated mode it connects only to the instance's socket, and config
validation refuses `[aether] socket` or `address`.

- `archon daemon stop` stops `archond`.
- `archon daemon stop --aether` (alias `--all`) then also runs `aether stop`
  against the instance.
- `archon daemon status` shows both daemons.
- `archon tui` connects/starts archond and the selected Aether instance, then
  sends operator-only `OrchestratorAttach`. The supervisor ensures a live
  orchestrator under its lifecycle lock and returns a strict snapshot in
  `Reply::Orchestrator`; unavailable links, backoff and standby are errors.
  The CLI retries transient unavailability within a five-second startup
  window, drops its client, and execs `aether tui --agent <resolved-id>`.
  Native `--theme`, `--ascii`, `--reduced-motion` and `--plain` options are
  forwarded unchanged. There is no additional renderer or refresh loop.
  Aether can fall back if the selected agent disappears after exec; exit and
  rerun to reattach.
- A bare `archon` does what `archon tui` does with default options. Where the
  TUI cannot run it opens `archon chat` (§12) instead, with one line on
  stderr saying why: stdin or stdout is not a terminal, or `[aether] address`
  (TCP) is set. `cmd::bare::front` decides from those two facts alone. Any
  other failure to open the TUI is an error that names `archon chat` and
  `archon doctor`, never a fallback. Nothing can be added to Aether's own
  words after the exec, so a bare `archon` first asks `aether tui --help` and
  refuses an Aether that lacks an option it passes; `archon tui` hands over
  unasked. `--json` is refused before either opens.
- `archon aether <args…>` runs any `aether` command against the instance,
  for example `archon aether agents`. It refuses to run before
  `archon setup`, because Aether would otherwise write default templates
  into the empty directory.

Because the instance's config always declares the server before its daemon
first starts, a new install never needs a restart.

**Keeping the registration current.** An upgrade can need something of the
registration that an older `archon setup` did not write; today that is
`send_caller_meta = true` (§11). The instance's config is Archon's own file,
so Archon repairs it without the operator, on every path an upgrade takes
(the installer, `archon update`, a source install): the repair is part of
the start check every command runs, not of a migration step.

- The check also asks whether the `archon` server entry lacks what this
  binary needs. If so the command takes the start lock, writes
  `<archon data>/aetherd-reload-owed`, and rewrites the entry in place
  (`migrate_archon_server`: the field is set and missing tool classes are
  added; the entry's other settings and every other section are kept, the
  file's leading comment block too; other comments are lost, in a file
  Archon generated). The result is parsed and checked before an atomic
  write. A config that does not parse is left alone and reported by `archon
  doctor`.
- Aether reads MCP settings only as it starts, so an `aetherd` that already
  answers still has the old registration. While the marker exists, a
  listening `aetherd` is stopped (`aether stop`, waiting up to 15 s for the
  socket to go quiet) and started again; the marker is removed once no
  `aetherd` runs with the old registration. The marker is written before
  the file changes, so a crash in between costs one restart too many, never
  one too few. `archon setup` and `archon setup --resync` do the same.
- An `aetherd` that has just stopped answering may hold its store a moment
  longer, and the `aether daemon` that follows then exits with `another
  aether daemon is already running`. On that one error the start is retried
  every 250 ms for up to 15 s; any other failure is returned at once. This
  also covers `archon daemon stop --aether` followed directly by a command
  that starts it again.

Not covered: a config that was brought up to date by hand or by an older
binary's `--resync` while `aetherd` kept running leaves no marker, so
nothing tells Archon the daemon is stale. `archond` notices the unnamed
calls (§11) and its notice names the restart: `archon daemon stop --aether`,
after which any command starts it again.

**Shared mode.** `[aether] mode = "shared"` keeps the earlier behaviour.
Archon uses the operator's `aetherd` (`[aether] socket`/`address`, else
Aether's own resolution, §4.1). `archon setup` appends the block below to
the operator's `$AETHER_CONFIG_DIR|~/.config/aether/config.toml` if no server
named `archon` exists yet, then asks the operator to restart `aetherd`. The
bench's private `archond` uses shared mode, pointed at the Aether under test.
Shared mode refuses the operator's `/answer` chat command (§5.2).

```toml
# Added by `archon setup`. Remove this block to unregister Archon.
[[mcp.servers]]
name = "archon"
command = "/abs/path/to/archon"
args = ["mcp"]
classes = { list_pipelines = "read_only", list_repos = "read_only", model_policy = "read_only",
            list_queue = "read_only", get_item = "read_only", get_stage = "read_only",
            queue_status = "read_only", cost_report = "read_only",
            model_evidence = "read_only",
            enqueue = "mutation", update_item = "mutation", cancel_item = "mutation",
            retry_item = "mutation", pause = "mutation", resume = "mutation",
            notify = "mutation", ask_operator = "mutation", publish_report = "mutation",
            register_repo = "mutation", autonomy_policy = "read_only",
            set_directive = "mutation", clear_directive = "mutation" }
```

Within one daemon, MCP servers are global, so every agent of the daemon that
runs `archon mcp` can see `mcp__archon__*`. In dedicated mode those agents are all Archon's. Archon
narrows this in three ways:

1. Stage agents get `tools: except [mcp__archon__…]`: every Archon tool, or
   only the mutating ones for an item with `tools = yolo` (§18.4).
2. Tool calls are treated as orchestrator calls, except `message_orchestrator`,
   which requires stage-agent caller metadata (§5.5). `archond` lets a
   mutating tool through only for the current orchestrator, by the caller
   Aether names (§11).
3. Operator-only actions (raising an autonomy ceiling, caps, repo removal,
   chat messages, answers to questions, hard pause) are not exposed over MCP
   at all.

In shared mode, a non-Archon Aether agent can read with these tools. Through
a registration without `send_caller_meta`, which cannot tell callers apart,
it can change Archon only while the orchestrator in charge cannot delegate
(§11, §17).

On the `claude-code` engine Aether serves its harness tools and every MCP
server's tools through one `aether` MCP bridge, so claude names Archon's tools
`mcp__aether__mcp__archon__<tool>`. `ToolExposure` uses Aether's names
(`mcp__archon__<tool>`) and filters the bridge. Live runs confirmed that the
orchestrator sees all of Archon's tools. Guarded stages now expose only
`message_orchestrator`; yolo stages also expose the read-only tools.

## 5. Data flow

### 5.1 Operator request to shipped PR

1. The operator types in `archon chat`. The CLI sends `Say {text}`. `archond`
   appends `FeedEntry::Operator` and calls Aether `send_message(orchestrator)`.
   Its `inbox.delivered` event makes the chat await the reply (§12).
2. The orchestrator reads the catalog (`list_pipelines`, `model_policy`,
   `list_repos`, `autonomy_policy`), plans, and calls `enqueue` once per unit
   of work with the autonomy knobs it chose (§18.1) and, for the stages
   whose model it has a reason to choose, a `model_plan` entry with an
   optional `model_reasons` line. Archon fills the stages left out (§7.4).
   It replies to the operator in its final turn text, which
   becomes `FeedEntry::Orchestrator`.
3. The dispatcher (§7) selects the item and moves it to `Dispatching`. It
   creates the worktree and branch, creates a `PipelineRun`, and asks the
   `RunDriver` for its first actions.
4. For each `EngineAction::Spawn`, the daemon checks agent capacity, persists a
   `StageRun{Spawning}`, calls `AgentBackend::spawn`, then persists the Aether
   ids and marks the stage run `Running`.
5. A `task.completed`, `task.failed` or `task.cancelled` event for a stage
   task makes the daemon read the outcome with `get_task`, persist it with its
   `Verdict`, and feed it to `RunDriver::on_stage_finished`. The driver returns
   the next actions: spawn the next stage, loop back with feedback, wait for
   the operator at a gate (§18.3), have archond deliver the change (§8.7),
   or finish.
6. When the run finishes, the item reaches a terminal state. The daemon
   appends a feed `Blocker` when operator action is needed, otherwise an `Outcome`, triggers the orchestrator with `item_finished`,
   and releases capacity so the dispatcher runs again. The orchestrator may
   follow up (enqueue a fix, notify a decision). It no longer needs to post the
   outcome itself, because the daemon already did. The notification text is
   clipped at 2000 characters and the `item_finished` summary at 4000; a
   clipped summary says so. A succeeded or failed item's also names
   `get_stage` with its run, which reads every stage's output in full (§11);
   a cancelled item's is its cancel reason, so it names nothing. A compacted batch keeps at most ten events besides
   pinned ones (§5.4), so its summaries stay near 40,000 characters.

### 5.2 Questions

- `ask_operator` (MCP) does not block. `archond` records a `Question` and a
  feed entry, with the caller Aether vouched for as its `asked_by` when the
  call carried caller metadata. When the operator answers (`archon answer <q> <text>` or
  `/answer` in chat), `archond` triggers the orchestrator with
  `operator_answer`.
- The store gives every `Question` a permanent `number` as it records it
  (store migration 4 numbered the existing ones in asking order). Numbers are
  never reused, so `Q7` names the same question in `archon questions`,
  `archon answer 7` and the chat's `/a 7`, and a number whose question has
  closed is refused instead of resolving to another one.
- Aether `ask_user` blocks. `archond` sees `question.asked` for the
  orchestrator or a stage agent and records a `Question` with
  `aether_question = {agent, seq}`. The operator's answer goes back through
  `reply_to_question`.
- An operator gate (§18.3) asks a `Question` too. `archond` decides the gate
  from the answer itself and does not trigger the orchestrator.

An answer is delivered first and recorded after. `archond` closes the
question and echoes the answer to the feed only once the reply, the gate
decision, the reopen or the `operator_answer` trigger has gone through. If
delivery fails (for example, Aether is unreachable), the operator gets the
error and the question stays open, so answering again retries it. Answers
are handled one at a time, so each question is delivered once.

A "Reopen it?" answer (§14) is the exception: it is recorded first, in the
same step under the store lock that checks its report still asks it, and
the page is reopened after. A withdrawal of that question checks the same
thing in the same way, so exactly one of them closes it. A losing
withdrawal does nothing. A losing answer is refused with the reason (for
example, that its report's gate closed), and nothing reaches the
orchestrator.

**Answers from the orchestrator's Aether chat.** The operator can be in the
orchestrator's chat with neither the `archon` CLI nor the localhost Lavish
page at hand (Aether's remote app is that case), so `archond` takes an
answer from there too. There is one answer path, `handlers::answer_question`;
`archon answer` and the chat command both end in it and differ only in the
`AnswerVia` recorded on the `Answer` feed entry (§12).

- The orchestrator pump (§4.6) already classifies each `inbox.delivered` on
  the orchestrator's stream for reply detection (§12):
  `from_operator = kind == "message" && actor == "user"`. The chat command
  reuses that one check. When it holds and the message's whole text, trimmed,
  is `/answer <number, Q<number> or q_… id> <answer>`, `archond` applies it.
  An agent's message (`actor = agent`), a relay or any other Archon news, and
  a GitHub request (all `external_trigger` items) never match, and a
  `/answer` embedded or quoted in a longer message is not the whole text.
- The orchestrator still receives the message. Its instructions say that the
  daemon applied it: it acknowledges in one line and neither answers the
  question again nor treats the text as a request.
- The outcome is never silent. `archond` pushes a `chat_answer` event (§5.4)
  saying what was applied, or why not: a malformed command, an unknown
  number, a closed question, or a refusal of the answer path. The
  orchestrator restates it in the same chat. A refusal is also a `System`
  feed entry. Only a store failure is returned to the pump (§16).
- The command is applied at most once: the pump marks the event seen before
  applying it, as it does for reply detection, so a replay cannot answer
  twice. A crash between the two drops the command; the question stays open.
- **Dedicated mode only.** In Aether, a connection that authenticates with
  the `[listen]` token is recorded as the same `user` actor as the operator
  at the Unix socket (`aether-daemon` `authz`: `AuthContext::Token` becomes
  `Actor::connected_user`); only a paired device with the observer role is
  a different actor (`client`). Archon's own instance has no `[listen]`
  (§4.7), so there `user` is the Unix socket, which the same OS user as
  `archond`'s own socket guards, or a device the operator paired with the
  operator role. In `mode = "shared"` the token would widen who can pass a
  gate, so the command is refused there with that reason, and `gate_opened`
  names `archon answer` instead. No setting changes this.

`record_chat_answer` (MCP, the orchestrator only) closes a question the
orchestrator asked with `ask_operator` once the operator's plain chat reply
answered it, with the operator's words and `AnswerVia::ChatReply`. Nothing is
routed and no `operator_answer` follows, since the orchestrator read the
reply itself. The caller must be the current orchestrator, by Aether's
out-of-band caller identity (§5.5), and must be the agent that asked:
`ask_operator` stores that same caller identity on the `Question`
(`asked_by`), and the two ids have to match. A question another agent asked,
one an earlier orchestrator generation asked, and one with no recorded asker
(asked without caller metadata, or stored before `asked_by` existed) are
`forbidden`; unknown ownership never authorises closing. So is a question
anyone else waits on: an Aether `ask_user` question, an operator gate's
question, and a report's "Reopen it?" question, the three cases the answer
path routes elsewhere. The orchestrator therefore has no way to answer a
gate, and the operator's `/answer` or `archon answer` closes what it cannot.

### 5.3 Spend

`agent.usage_updated` carries **absolute** totals per agent. The ledger stores
one `(agent, cost_usd, cost_basis, estimated_cost_usd, tokens)` row per agent
with what has been charged for it, and splits each charge into the local days
in which it grew. Aether prices a turn at list price while it streams and
replaces that with the provider's figure when the turn ends, so a total may
come down. A *settled* total (`cost_basis = provider_reported` with
`cost_complete = true`) is the agent's charge, and all of it is billed. It
never goes below the highest settled total before it (`spend.settled_cost_usd`,
schema V9), so reporting less cannot evade a budget. What it takes back comes
out of that agent's own days, newest first, so no day is credited with what
another agent spent in it. Any other lower total keeps the cost already
charged and is logged as a warning; the estimate is never settled and only
grows. `GetItem` reports each
stage's cost from its agent's ledger row plus the rows of the agents it
delegated to, and derives each run's `cost_usd`
from that run's stages and delegated descendants on every read, including terminal runs. Late usage
therefore remains with its own stage and run across retries and restarts.
A stage that delegated also reports the split (`delegations` in `GetItem`,
`delegation` in `GetStage`): its own agent's cost, the delegated agents'
cost, and each delegated agent with its parent, model, effort and cost.
Schema V8 (`delegated_agents`) records who delegated to whom as the watcher
links each child, and the child's effort when the stage's owed stop is
confirmed (§6.3); until then the effort reads as unknown. A run's and an
item's totals sum each agent once, so the split adds nothing to them. Model
evidence compares the same complete stage cost.
The persisted finish-time run cost is retained for compatibility, but is not
authoritative for item detail. Delegated children count toward item/global
budgets and their own run totals, but have no stage rows. Schema V6 records
agent-to-run ownership before children spend, including descendants created
before the stage spawn reply. This ledger ownership outlives watcher pruning.
Legacy agents are backfilled only where the item has exactly one run; old
child spend on an item with multiple runs has no recoverable run attribution
and remains in the item/global totals.
With `limits.count_estimated_cost = true` (the default), separate API-equivalent
estimates count toward budgets. A budgeted stage gets an Aether hard
`cost_budget` only when `list_models` pricing and the `providers` protocol prove that Aether
reports dollar cost: Claude Code reports it, Codex does not, and an HTTP
model needs pricing. Provider names may be customized. Protocols v14 to v17
expose no explicit `reports_cost` flag; missing model/provider metadata or
an unknown engine conservatively omits the hard cap and requires estimates.
The decision is persisted before creation so a keyed retry keeps its frame;
Archon's original grant remains stored for accounting and team replacements.

Stages always state their primary model and no fallbacks. Aether inherits a
parent's fallback chain only alongside an inherited primary, so no fallback
can bypass this budget decision. Introducing stage fallback chains must
check every link before enabling an Aether hard cap.

Without a provable hard cap, disabled estimates refuse the stage with a
budget explanation. Without a hard cap, if a budgeted item's usage has
neither complete cost nor a counted estimate, Archon fails the run visibly and cancels its live
agents; configure pricing in Aether or select a cost-reporting model. Usage
that exhausts an item budget also stops its live run. Existing admission
and stage/replacement checks continue to count the ledger. Accounting is
event-driven: work already in progress can overshoot before its usage event
arrives. An unbudgeted stage makes no capability queries.

`archon cost` and the `cost_report` tool (`CostReport`) report the ledger
over a range of local days, grouped by day, repository (by name), pipeline,
model (`provider/model`), item or stage. `--since` and `--until` take a date
(`2026-09-01`) or whole days back from today (`7d`, `2w`): as a start, `7d`
is the last 7 days with today; as an end, `1d` ends yesterday, so `since Nd`
and `until Nd` never overlap. Hours are refused, since the ledger keeps days.
The default is the last 7 days, grouped by day. One SQL query sums
`spend_daily` for the rows and another for the total, in one read
transaction; only a grouping by owner joins the items, repositories and
stage runs. Each row and the total show provider-reported billing (`cost_usd`),
list-price/API-equivalent estimates (`estimated_cost_usd`), unclassified cost
(`unclassified_cost_usd`), and the budget-counted amount (`counted_usd`).
Aether's raw `cost_usd` is classified by `total.cost_basis`: `provider_reported`
is billing, `list_price` is an estimate, and `unknown` or `mixed` is
unclassified because no trustworthy split is supplied. Separate raw
`estimated_cost_usd` always contributes to the estimate column. Schema V5
labels existing agent rows `unknown`. Schema V7 records billed, list-price and
unclassified growth separately in each daily row. Later usage with a changed
basis cannot relabel amounts already charged, with one exception: a settled
total says every dollar of the agent's cost was provider-reported, so it
moves that agent's days to billed.
Mixed growth stays unclassified because Aether supplies no component amounts.
Daily rows predating V7 migrate as unclassified: the mutable cumulative basis
cannot recover their historical provenance. Raw costs and separate estimates
are preserved, and new growth is classified using its observed basis.
These labels do not change enforcement: raw `cost_usd` counts regardless
of basis, even with estimates disabled; only the separate API-equivalent
`estimated_cost_usd` is controlled by `limits.count_estimated_cost`.
CLI status and MCP `queue_status` spend lines name these budget-counted totals.
A day's counted amount is what the daily budget counts for that day, and over
every day the total is what the total budget counts. Rows that do not add up to the total
fail the report instead of misstating spend. Spend with no owner in the
grouping (the orchestrator's, or an agent outside a stage for model and
stage) is one group with no key, shown as `-` in the CLI and `(none)` to the
orchestrator. The orchestrator's spend stays there under `model` too: the
ledger does not record the model it ran on, and the configured one may have
changed since. `CostReport` is served in standby too.

### 5.4 Trigger payloads to the orchestrator

`trigger {agent: orchestrator, source: "archon", payload}`. Triggers are
coalesced for `orchestrator.trigger_coalesce_ms` (default 2000) into one
payload. Idempotency key: `trg_<first feed seq>_<last feed seq>`. Each event
is saved in kv `trigger_outbox` when it is pushed, before any coalescing, so
news waiting in the window survives an `archond` stop or crash. The outbox
also keeps the keyed batch, so a restart resumes it under the same key.
After a quiet spell, delivery waits the window from the first event. News
the operator waits on or must act on ends the window at once: an
`operator_answer`, a `chat_answer`, a `gate_opened`, an `item_blocked`,
`lavish_feedback`, a `budget` and a stage's escalation
(`TriggerEvent::is_urgent`). News pushed while a batch is delivered has
waited already and goes out right after it.

**Which news wakes the orchestrator.** A wake is a whole turn of the
orchestrator over its context, so the daemon spends one only on news the
orchestrator has something to do about. Every finished item reaches the
operator from the daemon itself, as an `Outcome` or `Blocker` feed entry
(§5.1), and its `item_finished` news is then either *waking* or *quiet*
(`executor::finish::announce`):

| Outcome | News |
| --- | --- |
| Failed, or the operator must act (a `Blocker`, a `BLOCKED:` result) | wakes |
| Succeeded with an `origin`: a GitHub thread is owed the result | wakes |
| Succeeded and tagged `notify` by whoever queued it | wakes |
| Succeeded otherwise | wakes unless `wake_on_success = false` |
| Cancelled | quiet |

A success wakes by default because Aether's terminal UI shows only what the
orchestrator says, and Aether 1.0 has no way to add an entry to an agent's
chat without starting its turn.

A waking failure also carries `lesson_hint` (§19). An initiative's member
makes no `item_finished`: its initiative's `initiative_finished` wakes once
it is done or failed (§8.9).

Every other event wakes: `agent_message`, `item_blocked`, `gate_opened`,
`operator_answer`, `chat_answer`, `lavish_feedback`, `budget`,
`policy_violation`, `directives_restated`, `initiative_finished`. A mirror's change of state
(§13.3) is quiet. Quiet news is saved in the outbox like any other and makes
no trigger by itself. It leaves with the next trigger there is, and an
operator message sends it at once, without the coalescing window
(`triggers::flush_quiet`, called before the message is delivered), so the
turn that answers the operator has read it. When it is keyed, the quiet
outcomes become one `items_finished` event at the end of the batch: item,
title, state, pull request and cost of the newest outcome of each item, at
most 20, with `more` counting the rest, and without an outcome the item no
longer shows (it was retried since).

An `agent_message` is one feed entry the daemon writes with the news, in
the same transaction (§5.5), so the orchestrator does not echo it: an
escalation shows in the quiet chat as a `Blocker`, a relay or info with
progress.

While the orchestrator is down this news is held. A new, revived or
adopted orchestrator gets a retry at once, and the held batches reach it in
order, each once under its key. `archon status` (and its `held_news` field)
shows how many events wait. Item outcomes do not wait: their `Blocker` or `Outcome` feed
entries reach the operator chat directly.

A long outage must not replay a day of history, so held news is compacted
when it is keyed into a batch. That happens when more than 10 events wait,
when any of them was pushed over an hour ago, or when news waited while
the supervisor created a fresh orchestrator generation: the next batch
then holds news the new agent inherits. News pushed after the generation
was created is news to it and is compacted only as usual. Adopting
Aether's successor or waking the agent in place keeps its context, so
neither forces compaction. Compaction:

- always keeps every `operator_answer`, `agent_message` and `chat_answer`,
  every `lavish_feedback` whose report is still `open` in Lavish, every
  `gate_opened` whose question is still open, and every current
  `item_finished` with an `origin`, whose GitHub thread awaits the outcome;
- drops a `gate_opened` whose question was answered or withdrawn;
- drops an `item_finished` when a later one exists for the item, or the
  item no longer sits in the state it reports (it was retried or is gone),
  and an `item_blocked` when the item is no longer blocked;
- drops any other event older than an hour;
- then keeps the newest of the rest, up to 10 events in all.

Quiet outcomes are folded into their `items_finished` list before this and
are not counted against the 10.

What was dropped is counted by type in one `news_digest` event at the head
of the batch, which points to `list_queue`, `get_item` and `archon log` for
the detail. Nothing is dropped, and no digest is sent, when every event is
kept. Only news not yet keyed is compacted: a batch in flight is never
rewritten, so a retry under its key sends the same payload.

News held behind a stuck batch is also compacted by the same rules when a
push takes it past 40 events, so the outbox stays bounded however long the
batch in flight is retried. A later compaction folds that digest's counts
into its own, so one batch never carries two digests.

```json
{"archon": 1, "events": [
  {"type": "item_finished", "item": "qi_…", "title": "…", "state": "succeeded",
   "summary": "PR #42 opened: …", "pr_url": "…", "cost_usd": 3.12,
   "origin": {"repository": "owner/name", "number": 7}},
  {"type": "item_blocked", "item": "qi_…", "reason": "dependency qi_… failed"},
  {"type": "operator_answer", "question": "q_…", "text": "…"},
  {"type": "gate_opened", "question": "q_…", "number": 7, "item": "qi_…", "title": "…",
   "subject": "proposal", "asks": "Approve the direction proposed for this item?",
   "summary": "…the stage's summary, at most 600 characters…",
   "page": "http://127.0.0.1:4455/session/…",
   "reply": "`/answer 7 approve` (a note may follow), `/answer 7 reject` to cancel the item, or …"},
  {"type": "chat_answer", "number": 7, "applied": true, "text": "approve"},
  {"type": "lavish_feedback", "report": "rpt_…", "item": "qi_…",
   "prompts": ["…"], "text": "…the poll output without its dom_snapshot…"},
  {"type": "budget", "scope": "daily", "spent_usd": 51.0, "limit_usd": 50.0},
  {"type": "policy_violation", "agent": "agt_…", "model": "…", "action": "cancelled"},
  {"type": "mirror", "repo": "api", "state": "stale", "detail": "…git's error…"},
  {"type": "directives_restated", "reason": "your context was compacted"},
  {"type": "news_digest", "dropped": {"item_finished": 12, "item_blocked": 3},
   "note": "15 older or no longer actionable news events were left out. …"},
  {"type": "items_finished", "more": 4, "items": [
    {"item": "qi_…", "title": "…", "state": "succeeded", "pr_url": "…", "cost_usd": 0.41}]}
 ],
 "directives": [{"id": "dir_…", "text": "auto-merge docs fixes today", "source": "operator",
                 "set_at": "…", "expires_at": "…"}],
 "directive_changes": {"set": [{"id": "dir_…", "text": "…"}], "cleared": [{"id": "dir_…", "text": "…"}]},
 "directives_in_force": 3,
 "summary": "9 events: item finished: qi_… succeeded, item blocked: qi_…, …"}
```

A payload carries `directives` or `directive_changes`, never both, and
neither when nothing changed (below).

The top-level `summary` is a derived display label, at most 160 characters,
with whitespace collapsed and Unicode control/format characters removed.
It names the single event or the count and labels of a batch. Full event
bodies and directives remain authoritative and are retained unchanged.

**Directives in a payload** (§18.5). Restating every directive in every
trigger was most of a typical payload, so they are sent in full once per
orchestrator generation and as changes after that. The first trigger of a
generation carries `directives`, all in force. A later one carries
`directive_changes` (`set`, `cleared`: withdrawn or expired) when something
changed since the trigger before, and no directive field when nothing did.
Every trigger carries `directives_in_force`, the count: an orchestrator
that holds fewer reads them with `autonomy_policy`. What the current
generation was told is kept in kv `trigger_directives_told` and decided when
a batch is keyed, so a retry under the batch's key sends the same payload.

Whenever the orchestrator may have lost them, the next trigger states them
all again and says why in a `directives_restated` event, and when any
directive is in force that trigger is sent at once, so no request is acted
on without them:

- a new generation (created, or replaced after a failure);
- a revival of the same agent from its checkpoint, which may predate them;
- Aether's `compaction.completed` on the orchestrator's stream: its older
  context became a summary.

An orchestrator an earlier Archon started has a prompt that expects every
directive in every trigger, so it keeps getting them in full: kv
`trigger_directive_changes_from` holds the first generation this build
started, and only that one and later ones are sent changes.

`gate_opened` is pushed when an operator gate's question is recorded
(§18.3), under the same store lock, so the orchestrator can restate the gate
in its chat as text: `number` is the question's `Q<n>`, `subject` is
`proposal` or `report`, `page` is the Lavish URL or file path on `archond`'s
machine, and `reply` spells the answers out in the vocabulary of
`GateDecision::from_answer`, as `/answer <n> …` in dedicated mode and
`archon answer <n> …` in shared mode (§5.2). `title` is clipped to 200
characters, `summary` to 600 and `page` to 300. `chat_answer` reports what
`archond` did with an operator `/answer` (§5.2): `number` is absent when the
command named no recorded question, and `text` is the answer applied or the
reason it was not, at most 500 characters.

Stage-level progress never triggers the orchestrator. It stays in the store
and is visible with `get_item` and `get_stage`.

### 5.5 Messages from agents to the orchestrator

`message_orchestrator` is the stage-agent exception to Archon's mutation
tool restrictions: it is exposed in both guarded and yolo modes. Arguments
are required `text` (non-blank, at most 4096 Unicode characters) and optional
`kind` (`relay`, `escalation`, `info`; default `info`). The MCP server reads
the caller from Aether's out-of-band `params._meta["aether/agent_id"]`,
enabled by `send_caller_meta = true` in the Archon server registration.
The Archon Unix socket is owner-only (0600 inside a private directory):
caller metadata authenticates routing within that trust boundary, not against
arbitrary processes running as the same OS user. Socket clients of that user
can supply caller identity; the daemon checks it against recorded stage ownership.
Caller identity is not an argument the model can supply; missing metadata,
unknown callers and settled stage callers are refused visibly.

Archond resolves the caller's stage and records the message in the feed
for the operator to read as it is: `From <stage> (<sender>; <kind>): <text>`,
the whole text, as a `Blocker` for an escalation, which the quiet chat
shows, and as `Progress` for a relay or info. The orchestrator therefore
never repeats a message to the operator; it acts on it. The news event
carries item, pipeline run, stage run, stage, attempt, agent, kind, text and
original sender. One SQLite transaction commits the feed entry, the
sender's count and the `agent_message` news event in the durable outbox.
A sender's count is capped at `[limits] max_agent_messages` (20 by
default), and so is each run node's count of external messages; the
refusal names the key. The receipt is the feed entry id. The tool does no Aether
I/O and does not settle the stage. Stage instructions and the receipt say
to continue working: ending a turn would complete the stage, and sending
a message is never a reason to wait or end it.

The fleet pump parses message content and sender from `inbox.delivered`.
An authenticated Aether `user` sender becomes `original_sender.actor =
"operator"`; agent senders retain their id. A stage's `relay` gets operator
provenance only if its text exactly matches the latest operator message
the fleet pump observed in that stage's inbox. If observation has not
caught up, provenance stays with the stage agent. `relay` alone grants no
operator authorization. Escalations need an orchestrator decision; info
is low priority. Replies use existing Aether messaging, retry guidance or
new items, without making the stage wait.

Messages addressed to a passive run node follow the same recording and
news path, with `addressed_to_run_node` naming the recipient. The item and
run belong to that recipient; stage fields are included when the sending
agent belongs to a stage of that run. Operator inbox messages use `relay`,
other senders use `info`. A persisted per-node inbox cursor commits with
the event, so replay cannot duplicate it. Retired nodes remain routable:
pruning moves their normal index to a minimal routing tombstone retained until 14 days after run
completion.
For a node whose mapping was pruned by an older Archon (or whose create
reply has not been recorded yet), the pump recovers routing from Aether's
`archon-run` role and scope labels, validated against the stored run.

Both paths share a durable cap of 20 accepted messages per sending agent;
operator and system inbox messages have a separate cap of 20 per run node.
The tool returns a clear error above the cap. Invalid or excess run-node
messages produce an idempotent refusal to the sending agent, or a notice
in the operator feed. Accepted messages share the normal trigger coalescing
window, which an escalation ends (§5.4), and remain pinned through
compaction until delivery. Counts and
routing tombstones are kept through the late-message window. Message counts
remain durable so restarts or routing recovery cannot reset limits.

## 6. State machines

### 6.1 Queue item (`archon_core::queue::QueueState`)

```
            enqueue
               │
               ▼
   ┌──────▶ queued ──────────────┬──────────────┐
   │ retry    │ dispatcher picks │ dep failed/   │ cancel
   │          ▼                  │ cancelled     ▼
   │     dispatching ───────┐    ▼           cancelled ◀─┐
   │          │ run created │  blocked ──unblock──▶ queued
   │          ▼             │    └── cancel ──────────────┘
   │       running ─────────┼── cancel ──▶ cancelled
   │          │             │
   │   ┌──────┴──────┐      │ worktree/spawn error
   │   ▼             ▼      ▼
   ├ failed ◀──── (run failed)
   └ succeeded (run succeeded)
```

The legal edges are implemented in `QueueState::can_transition_to`:

- `queued` can go to `dispatching`, `blocked` or `cancelled`.
- `dispatching` can go to `running`, `failed` or `cancelled`.
- `running` can go to `succeeded`, `failed` or `cancelled`.
- `blocked` can go to `queued` or `cancelled`.
- `failed`, `cancelled` and `succeeded` can go back to `queued` (operator or
  orchestrator retry). A retried `succeeded` item continues on its branch into
  the pull request it has, and the retry is a correction of the run that had
  passed (§19).

When a dependency fails or is cancelled, its dependents move to `blocked` and
the orchestrator gets an `item_blocked` trigger. When a dependency is retried,
its blocked dependents move back to `queued`.

### 6.2 Pipeline run (`RunState`)

`running{stage_id, attempt}` ends in `succeeded{summary}`,
`failed{stage_id, error}` or `cancelled{reason}`. Between stages it may wait
in `awaiting_operator{stage_id, attempt, subject}` for an operator gate
(§18.3); it holds no agent slot there, and its item stays `running`. A run
belongs to exactly one item and an item has at most one non-terminal run.
A run stays `running` at its last stage while archond delivers its change,
and one whose delivery is refused fails with the stage id `ship`, a step no
agent runs (§8.7). A retry creates a new run: in the item's kept worktree, or, for a fresh retry,
in a new worktree off the base branch (§13).

### 6.3 Stage run (`StageState`)

`spawning` → `running` → `completed | failed | cancelled | timed_out`.

- `spawning` is persisted **before** `create_agent`, with the exact request
  in `kv.spawn_request:<stage run id>`; the run node it is parented under,
  and the network it states (no wider than the node's or Aether's default
  at that moment), are written into that request before the first send
  (§4.5). The request is read until the stage is terminal (its `deadline`
  drives the daemon timer below), then pruned (§16). How a failed
  `create_agent` is handled depends on the error:
  - No answer (`ArchonError::Unavailable`): re-issued under the same
    idempotency key with the same stored request, for as long as it takes.
  - A transport error (`Protocol`, `Io`) or Aether's `internal` code: a
    failure inside the link or inside Aether, not a verdict on the request.
    It is re-issued the same way, and after 5 failures in a row the stage
    run is `failed` with the last error.
  - A failure on a full disk (its text says `database or disk is full`,
    `No space left on device` or `ENOSPC`, whatever the code): the stage
    run stays `spawning`, and the disk hold (§7.3) engages. It is re-issued
    under the same key once the hold ends, so tries are at least the hold's
    5 minutes apart. After 5 such failures the stage run is `failed`, and
    its pipeline run fails with a `Blocker`: Archon's disk had room for each
    retry, so the full disk is likely Aether's, which Archon cannot read.
    If its pipeline run has finished, the stage run is `cancelled` at once
    instead: the run needs no agent, and Aether created none.
  - Any other refusal (`not_found`, `invalid_transition`,
    `invalid_argument`, `unavailable`, `incompatible`, `unauthorized`): the
    stage run is `failed` and the error is kept. A model policy refusal also
    posts a `Blocker` (§7.4).
  - A failure of Archon's own store stops the work (§16).
- `timed_out`: Archon sets the Aether `deadline`, and a daemon timer also
  cancels the agent at `deadline + 60s` if Aether has not settled it. An
  `agent.failed` whose event time is at or past the deadline (Aether's own
  `deadline of …` stop) settles `timed_out` too, never an `infra` re-spawn;
  one stamped earlier stays `infra` however late Archon receives it.
- `infra` failures (`agent.failed` before its task settles, or Aether restart
  recovery failures) get `limits.infra_retries` (default 1) automatic
  re-spawns. These retries do not count against a stage's `max_attempts`.
- A stage that failed on a full disk gets up to 5 re-spawns of its own.
  Its error text must say so, as above, and come from Aether's
  infrastructure (`infra`). A failure the agent reported itself counts only
  if a watched path also has less than 1 GiB free: text an agent wrote may
  quote an `ENOSPC` it merely saw, and then fails as the agent's own. They
  do not use up `limits.infra_retries` or `max_attempts`. The failure
  engages the disk hold (§7.3), and the replacement is reserved at once but
  issued only when the hold ends. Each slot counts its infra and its
  disk-full retries apart, by the error of each run it replaced.
- **What a delegating stage agent left running is stopped before the run
  goes on.** Aether cancels an agent's children with the agent, but not
  when its task completes or fails, so a lead that settled while a child
  still ran would leave it writing beside the next stage, or beside its own
  replacement. A stage run whose agent could delegate (its stored request
  has `leaf: false`, or the request is gone) therefore owes a stop when it
  settles, whatever the outcome: `kv.descendant_stop:<stage run id>`, with
  the run, the agent and the settle time, is written under the same store
  lock as the settle and before it, and before a replacement is reserved: a
  crash in between leaves a stop too many, never one too few. While a run
  has one, none of its `spawning` stages is issued.
  The dispatcher pays it in two steps. Once the agent's own turn has ended
  (it is left up to 60 s to end it, as a retiring run node leaves it,
  §4.5), Archon sends `cancel` for the agent, once: Aether applies a
  `cancel` of an agent that already ended to every live agent beneath it,
  at any depth, and keeps it pending until they have ended. Aether's
  answer (or `not_found`) is stored in the record as `cancelled_at`. The
  cancel is cooperative, so the answer does not mean anything has stopped:
  each later dispatcher pass lists the agent's Aether run once and keeps
  the record while any agent beneath it is in a non-terminal state. An
  agent's end wakes the dispatcher (§4.6), so the wait ends with the last
  of them rather than with the next tick. Only then is the record deleted
  and the stages that waited issued. No answer or a transport error, to
  the `cancel` or to the listing, leaves it owed, for the next dispatcher
  pass or the next `archond`. A stop still unconfirmed 120 s after the
  cancel posts one `Blocker` naming the agents that still run; the run
  keeps waiting and a `System` entry says when it goes on. Any other
  refusal of the `cancel` fails the run with a `Blocker`, and the run
  node's retirement then cancels the whole subtree. A leaf stage agent
  owes nothing.

### 6.4 RunDriver (pipeline engine)

The driver is a pure state machine: `(spec, run, stage_runs) × input → actions`.

```rust
pub enum EngineAction {
    Spawn { stage_id: String, attempt: u32, fan_index: u32, choice: ModelChoice, prompt: String },
    CancelStage { stage_run: StageRunId, reason: String },
    AwaitOperator(OperatorWait),      // a gated stage passed; §18.3
    MergeTeam(MergePlan),             // a team stage passed; §13.1
    Ship(ShipPlan),                   // every stage passed; archond delivers, §8.7
    Finish { state: RunState },
}
```

Per stage:

1. Spawn `fan_out` agents, using choices from `model_plan[stage]`. With fewer
   choices than slots, cycle through them.
2. Wait until all of them are terminal. A `gate = none` stage succeeds when
   every agent completed. For gated stages, the `Verdict`s of completed agents
   are evaluated: `all_pass` needs every verdict to be `pass`, `majority`
   needs more than half, and `any` needs at least one. `unknown` counts as
   fail.
3. On success, go to the next stage. After the last one, a pipeline with
   `[pipeline.ship]` emits `Ship` and waits for `on_shipped(outcome)`:
   `Shipped` finishes `succeeded`, `Failed` finishes
   `failed{stage_id: "ship", error}`. Any other run finishes `succeeded` at
   once. The summary is the last stage's summaries joined, then the
   delivery's `SHIPPED:` line. A stage the item's knobs gate
   (`RunDriver::with_gates`) first waits for the operator's decision, which
   `on_operator_decision` applies: go on, re-enter a stage with the feedback,
   or finish `cancelled` (§18.3). A revision resets the `on_fail` loop counts
   of the stage it re-enters and of every later stage.
4. On failure with `on_fail.goto = X`, while the loop count is below
   `on_fail.max_loops`, re-enter stage X with `attempt + 1` and
   `{{feedback}}` set to the failing agents' summaries and findings. When the
   loops are exhausted, or there is no `goto`, finish with
   `failed{stage_id, error}`.

Feedback, from a loop or an operator revision, always reaches the re-entered
stage: when its prompt has no `{{feedback}}` placeholder,
`sections::context` appends it under `## Feedback to address`.

A passed `team` stage does not advance at once: the driver emits
`MergeTeam` and waits for `on_team_merged(stage, attempt, outcome)`.
`Merged` goes on as step 3. `Conflict { role, files }` fails the stage as
step 4, with the conflicted files as the feedback (§13.1).

A team stage has no gate, so one role that settles other than completed
(failed, timed out, cancelled) dooms the attempt. The driver does not wait
for the other roles: it treats each still-running sibling as cancelled,
with the reason `cancelled because role `api` failed in team stage `build`
attempt 1` as its summary, and decides the stage at once. It emits a
`CancelStage` per sibling with that reason, once each (`cancelling`),
together with the decision: a loop's `Spawn`s, or `Finish`. The failure
headline names each role and how it ended (`role `api` failed, role `ui`
was cancelled`), and so does the `on_fail` feedback. A resumed driver owes
the same cancels until the stage runs are persisted cancelled, so a crash
between the decision and the cancels loses neither. `respawn` returns
`None` for a slot of a doomed attempt.

`RunDriver::resume(spec, run, stage_runs)` rebuilds the driver after a
restart. Every decision derives from persisted stage runs, recorded gate
decisions (`record_decision`), recorded team merge outcomes
(`record_merge`) and the recorded delivery outcome (`record_ship`), and the
driver has no hidden state.

An infra retry (§6.3) is not a settle: `RunDriver::respawn(failed, all)`
returns the slot's original `SpawnPlan` (or `None` if the run is already
decided), and the executor persists the failure and the new stage run in one
transaction. The newest stage run of a slot supersedes older ones.

### 6.5 Stage hand-off

A stage starts with what the stages before it reported, so it does not
spend its first turns finding that out again. `sections::context` appends
`## Handoff from earlier stages` to the rendered prompt: one entry per
earlier stage with a summary, and one for the stage's own last attempt when
it is re-entered. Stages the prompt already shows are left out: one its
template names as `{{previous.<id>.summary}}`, the proposal (it has its own
section, §18.3), and what the change report covers for a ship stage or
lists itself.

The digest is bounded: at most 1,200 characters per stage and 4,000 in all
(`HANDOFF_STAGE_CHARS`, `HANDOFF_TOTAL_CHARS`). When the budget runs out the
nearest stages are kept, and the entries stay in pipeline order. A stage a
steer skipped (§6.6) has no entry. The digest is prompt text, appended
after rendering, so a summary is never read as a template; the workspace is
stated to be the truth where the two differ.

### 6.6 Steering

A steer changes an item without cancelling it (`Request::SteerItem`,
`archon_core::steer::Steer`). The orchestrator steers through `steer_item`,
the operator through `archon queue steer`; a helper or a stage agent is
refused (§11). `Steer::check` refuses one that changes nothing.

| Field | On a running item | On an item with no live run |
| --- | --- | --- |
| `note` | Amends the brief (`## Amendment (<UTC time>)`) and the run's `task.brief`, so every prompt rendered from then on has it. Every live stage agent is sent it, and so is every live agent beneath one. | Amends the brief. |
| `model`, `effort` (+ `stage`) | Changes every choice of the stage in the item's plan, checked against the model policy (§7.4), and patches the stage's live agents. `stage` defaults to the running stage. | Changes the plan; `stage` is required. |
| `budget_usd` | Sets the item's cap. A live agent that carries a cap of its own gets the difference, a team's roles each their share. | Sets the item's cap. |
| `extend_minutes` | Moves the deadline of every stage run working now. | Refused. |
| `abort` (`then`: `retry`, `skip`, `goto`) | Ends the running attempt of `stage` as cancelled without judging it; the run then retries the stage, goes past it, or goes to the named stage. On a stage that has not started, only `skip`: the run passes the stage over when it reaches it. | Refused. |
| `insert_stage {stage, from?, after?}` | Puts a copy of stage `stage` of pipeline `from` (the item's own when left out) among the stages that have not started: after `after`, or before the first of them. | The same, for every later run. A failed item is queued again (below). |
| `switch_pipeline` | Replaces the stages that have not started with what the named pipeline runs after the stages both share. | The same, for every later run. A failed item is queued again. |

Refusals name the reason. A stage that already started keeps its shape: an
insert lands after the last started stage, a switch keeps every started
stage, and a stage that ran and is not running now can be neither aborted
nor removed (abort the running stage with `goto` to run it again). A stage
that passed and waits at an operator gate is decided at its gate, not
aborted. A switch on a run under way leaves out the new pipeline's
proposal stage, since the run is past it. A switch to a pipeline with
another workspace mode, or one that leaves nothing to run, is refused; so
is a team stage for a run that was started without team worktrees. A stage
an insert or a switch carries in that wrote its own pipeline's ship record
writes the run's when it runs after the stage the run shipped from so far
(`reshape::ship_after`), and an insert that would commit after the ship
record is refused. The result must pass the model policy and
the autonomy ceiling: a steer cannot take out the review an enqueue could
not leave out (§18.2). An item that is dispatching is refused with "steer
it again in a moment", a succeeded one always, and a cancelled one for a
shape change (retry it first).

**How a live run takes a steer.** `executor::steer::steer_run` holds the
run table. `plan::plan` works the steer out against the stored run with
nothing written, and builds the driver the result would leave
(`recover::driver_of`): a steer no driver can run is refused before
anything changed. `apply` then stores, under one store lock, the item, the
run's variables, the edits, the cancelled stage runs of an aborted group
with the stop their agents are owed, what each live stage run's agents are
owed, and the audit record. It rebuilds the driver from the store with
`recover::resume_run`, which is the path a restart takes. So a steered run
replays the same after a restart.

**Run edits.** A change to a run's stages is a `RunEdit` appended to the
`run_edits` table (`Store::append_run_edit`), numbered per run:
`InsertStage`, `SwitchPipeline` and `AbortStage {stage, attempt, then}`.
An insert or a switch carries the stages and the prompt texts it adds, so
replay never reads the catalog. `frozen::thaw` applies the run's edits in
order to its frozen pipeline (`reshape::apply_all`) before
`RunDriver::resume`, and `record_abort` gives the driver each abort. An
inserted stage whose id is taken gets a numbered one (`test-2`), and a
prompt file name that already holds another text becomes
`<id>-steered.md`. An aborted group is never judged: its `on_fail` is
not consulted, `retry` re-enters the stage with the feedback it had,
`skip` and `goto` clear the feedback, and `goto` back gives the stages it
re-runs their full loop budget. A removed stage is `AbortStage` with
attempt 0. It holds for this run; a later run of the item has the stage
again.

**What the live agents are owed.** Aether applies a patched model or effort
at the agent's next turn, not inside the running one: `update_agent`
(`aether-runtime` `commands/update.rs`) writes it to the agent, and a turn
reads the agent's model when it starts. A move within one engine keeps the
session and its history; a move between engines starts the next turn from
Aether's checkpoint of the context. So a model or effort steer is three
steps for the stage's own agent, in this order: `update_agent`, `interrupt`
to end the running turn, then a message that starts the next turn on the
new model and says so. A note, a budget and a deadline need no
interrupt. Each step is stored in `kv.steer_owed:<stage run>` in the same lock as the steer, with a unique
key that Aether dedupes the message by. A step Aether did not answer stays
owed and the sweep repeats it; a step Aether refuses is dropped with a
progress note on the item, and what the steer stored still applies to the
stage's next agents. A stage run that settled is owed nothing more. A steer
at an operator gate changes what the next stage starts with and leaves the
gate and its question as they are.

**An item with no live run.** The steer changes what the item runs next.
A shape change is kept as the item's stage list in `kv.shape:<item>` (a
`RunEdit` holding the whole list), which `start` and `get_item` apply over
the catalog's pipeline; a switch back to the item's own pipeline forgets
it. An item that already ran cannot change between a team layout and a
single worktree.

**Reviving a failed item.** A stage that fails with `ESCALATE:`, or fails
any other way, ends its run. An `insert_stage` or `switch_pipeline` on the
failed item says how it goes on: `executor::revived(failed_run, edit,
item)` amends the brief with the stage that failed, its error and the
branch the work is on, and the item is queued again as a plain retry is
(§13). Its next run works in the same worktree and on the same branch, with
the failed run's commits, and runs the reshaped stage list from its first
stage. The item keeps its pipeline's name; `kv.shape:<item>` holds what it
runs, and its `pipeline` names the catalog pipeline last switched to. Item
views give that as `switched_to` when it is not the item's own, and `queue
show`, `queue list` and `status` print `<queued> → <switched to>`.

**`update_item` on a running item.** `brief_append` is steered in as a
note. `model_plan` replaces the plan for the agents started from now, and
each live stage run whose slot gets another model or effort is patched as
above. While an item is dispatching both are refused.

**Audit.** Every steer is a `SteerRecord {at, by, what, note}` kept with
the item (`kv.steers:<item>`, the newest 100), a progress entry in the
feed (`Steered <item> (<by>): <what>.`), and a line in `archon queue show`
and `get_item`. A steer of a running run, and the start of a run whose
shape a steer set, record a `steered` correction of that run (§19).

### 6.7 One agent through a fix loop

A fix attempt by a cold agent pays for reading the task, the repository and
its own earlier work again. So the agent of a work stage that a later stage
can send back is kept, and its next attempt is the same agent told what to
fix.

`RunDriver::keeps_agent(stage)` holds for a `work` stage with one agent that
some stage names as its `on_fail.goto` and that does not hold the declared
review (a resumed attempt hands in no checked result). Its `SpawnPlan` has
`keep = true`, and the agent is created `long_lived`: Aether leaves it
waiting on its inbox when its task completes. Reviewers and every other
stage are created as before, a fresh agent per attempt.

- **Parked.** When an attempt of a kept agent completes, its deadline is
  moved 7 days out (`PARKED_DAYS`) and it waits. It is a child of the run's
  node, so the node's cancel ends it with the run (§4.5); the node's
  retirement does not wait for it to end a turn. An attempt that ends any
  other way has its agent stopped.
- **Resumed.** The next attempt of the slot, entered with feedback, takes
  the agent over (`resume::takeover`). `update_agent` gives it the stage's
  deadline and, when its creation set a cost cap, a cap of what its earlier
  attempts spent plus the new attempt's grant (`resume::regranted_cap`).
  `send_message`, keyed by the stage run, gives it
  `sections::resume_message`: the findings, and how to hand in. The stage
  run is `running` with the same agent and no task. Notes steered in while
  the agent was parked are added to the message.
- **Settled.** The agent's task settled with its first attempt, so a
  resumed attempt settles on the turn in which the agent took the message.
  `AgentBackend::resumed_outcome` reads the agent's own events: the
  delivery of the message, the turn that consumed it, and the last thing
  the agent said before going idle (`ports::resumed_reply_outcome`). A
  message starting `FAILED:` fails the attempt; one starting `BLOCKED:` or
  `ESCALATE:` ends the run as from a fresh agent (§8.6); anything else is
  the attempt's summary, with the object of its last fenced `json` block
  as its `structured`. That is how a kept agent of the stage `[pipeline.ship]`
  names hands in a new ship record (§8.7) for its fix, which
  `sections::resume_message` asks of it. A message without a valid record
  gets one more message, keyed `<stage run>:record`, asking for the record
  alone (`resume::recorded`); the attempt then settles on that answer's
  record with the first message's summary, and a second miss fails the
  stage at its gate. The pump checks when the agent's turn ends and the
  sweep checks again, so a missed event only delays it. A resumed attempt
  past its deadline has its agent cancelled and is `timed_out`.
- **Replaced.** A fresh agent takes the attempt, with the findings under
  `## Feedback to address` and the digest of the last attempt (§6.5), when
  the kept one cannot go on: Aether no longer holds it live, Aether refuses
  the deadline or the message, a steer gave the stage another model or
  effort, the stage is re-entered without findings (a steer's `goto`), or
  the resumed attempt was lost to an infrastructure failure. The kept agent
  is stopped before the fresh one is created, so the fresh one has the
  worktree to itself: the agent to stop is stored with the reserved spawn
  (`StoredSpawn.replaces`), and stopping one that already ended is done.

The stage run rows keep this replayable. `stage_runs.aether_agent` is
unique, and a kept agent passes from a finished attempt to the next attempt
of the same slot (`claim_agent`); the finished row still names it in its
data. After a restart a resumed attempt is found running with an agent and
no task, and is read as above. Spend follows the agent to the attempt that
holds it.

A steer for a resumed attempt patches and messages the agent but does not
interrupt it, since the attempt settles on its turn's last word; the
attempt then settles on the turn that took the steer's message.

## 7. Scheduling, caps and budgets

### 7.1 Capacity unit

The unit is **concurrently running stage agents**. A fan-out stage with 3
reviewers occupies 3 slots. A pipeline run between stages occupies 0 slots.
The orchestrator is not counted, and neither are the agents it delegates to
(§4.4): no cap bounds them, though their spend counts toward the budgets.

### 7.2 Caps (`Caps`)

| Cap | Meaning | 0 means |
| --- | --- | --- |
| `max_parallel_agents` | Global running stage agents. | unlimited |
| `max_per_repo` | Running stage agents per repository (repo config may lower it). | unlimited |
| `max_per_pipeline[name]` | Running stage agents per pipeline. | unlimited |
| `max_active_runs` | Non-terminal pipeline runs (bounds worktrees and disk). | unlimited |
| `budget_usd_per_item` | Default item budget (the item's own `budget_usd` wins). | unlimited |
| `budget_usd_daily` | Spend in the local calendar day. | unlimited |
| `budget_usd_total` | Lifetime spend recorded in this store. | unlimited |

### 7.3 Dispatch (`archon_queue::plan_dispatch`)

A pure function. It takes a `DispatchSnapshot` (queued items with their
dependency states, active runs with pending spawns, running agent counts per
repo and pipeline, pause state, spend) plus `Caps`, and returns a
`DispatchPlan`:

1. Pending spawns of **already running** runs come first, in item priority
   order and then by item age. Finishing work beats starting work.
2. Then queued items whose dependencies all `succeeded`, ordered by `priority`
   descending and then by `created_at`, skipping paused repos and pipelines.
3. A spawn is admitted only if the global, repo and pipeline caps all have a
   free slot. A fan-out stage may start partially and fill later. A team
   stage (a gang: `PendingSpawn.gang`, `QueuedCandidate.gang`) is admitted
   whole or not at all. A gang that does not fit holds the caps too full
   for it (`GangWaits`): later spawns and items under any of them are not
   admitted this pass, so a stream of smaller work cannot starve it, while
   work under other caps still flows. A gang bigger than its effective
   cap (`effective_agent_cap`: the smallest non-zero of the global, the
   repository's and the pipeline's) can never fit and holds nothing back.
4. Nothing is admitted while global pause is on, while the disk hold is on
   (below), or while daily or total spend has reached its budget. Hitting a
   budget posts one `Blocker` to the feed and a `budget` trigger. The holds
   are checked in that order, and `held_by` names the first that applies
   (`GlobalPause`, `LowDisk`, `TotalBudget`, `DailyBudget`).

**Carrying out a plan.** Independent work runs in parallel lanes
(`executor::lanes`, at most 8 at once), and each lane runs its work in
order. Starts and ships are laned by repository, because git writes a
repository's shared state (its worktree list, `.git/config` on
`push --set-upstream`). Spawns are laned by pipeline run, because a run's
first spawn creates its Aether run node. So one slow Aether call or
`git push` holds back only its own lane.

**Derived waiting reasons.** `dispatch::waiting_reasons` calls the same pure
planner and uses its admissions to distinguish `capacity` from `next_in_line`.
It shares slice-pause and global-hold predicates with dispatch, accounting for
pending spawns, team fairness and slots reserved for higher-priority/older work.
The daemon builds the complete snapshot through `dispatcher::read_snapshot`,
using the dispatcher's repository prefetch and run-table/store lock order. A
read reserves no slots, checks no disk and changes no persistent state.

`ItemSummary` and `ItemDetail` add optional `waiting_reason` with a stable
snake-case `code`, `message` and one `next_action`; stored `QueueItem` is
unchanged. `DaemonStatus.waiting_items` contains queued, blocked and currently
operator-waiting summaries. MCP `list_queue` requests summaries (item fields
remain flattened); `get_item` uses detail. Both expose reasons in text and
structured content. CLI list is short, detail includes the explanation, and
status puts waiting rows before the daemon summary.

An otherwise `next_in_line` item uses `aether_disconnected` while Aether
is unreachable, with `archon doctor` as its next action.

Explanation precedence: `operator_gate`, `dependency_failed`,
`dependency_waiting`, `global_pause`, `low_disk`, `total_budget`, `daily_budget`,
`repo_pause`, `pipeline_pause`, `capacity`, `next_in_line`. A blocked item with
no unmet dependencies uses `blocked` and points to its recorded detail; retry
of a failed/cancelled dependency normally requeues eligible blocked dependents
(§7.4). Dependency explanations name the first relevant dependency in the
item's dependency order. Answered/withdrawn gates, advisory questions, old
runs and terminal items do not imply a wait. If the queue or dependencies
change between the admission snapshot and item read, a new queued item
missing from the snapshot has no waiting reason in that reply. Dependencies
that change during explanation still return a retryable `unavailable` error.
A team constrained by agent caps gets actions for every cap below its required
team size, including ties.

**Disk hold.** Every dispatch pass, and once before recovery, `archond`
reads the free space (`statvfs`, the bytes an unprivileged process may
write) of the filesystems holding the data dir and the worktree root, and
keeps the lower. The planner sees the result as `DispatchSnapshot.low_disk`.
Two things turn the hold on:

- Low space: less than `limits.min_free_gb` (default 10 GiB) free. It
  ends as soon as that much is free again. `min_free_gb = 0` turns this off.
- A failure on a full disk: a stage agent that failed with one, a
  `create_agent` refused with one, or Archon's own store (§16). It holds for
  at least 5 minutes, and until `min_free_gb`, and at least 1 GiB, is free,
  even with the low-space hold off.

While the hold is on, no item starts and no stage agent of a live run is
issued, not even a reserved replacement or one recovery would re-issue.
Those stay `spawning` and are issued by the first sweep after the hold
ends. The spawns of a finished run are still issued, so a run cancelled
during the hold stops its agents and retires at once: a never-sent spawn
settles `cancelled` without reaching Aether, and one that may already have
reached Aether is sent again under its key, and the agent it finds is
cancelled. During a disk-full hold only the never-sent ones are issued: the
others wait for the hold to end like the rest, since sending them to a
full disk would only fail again. The trade-off is that an agent whose
`create_agent` reply was lost may keep running until the disk-full hold
ends. A low-space hold alone is no such failure, so it keeps them issued
and such an agent is cancelled at once. Running agents finish. Each hold
posts one `Blocker` naming the free space, the path, the cause and
`archon gc` (`kv.dispatch_disk_hold`: `low`, `full`, `full_again` or empty).
A disk-full failure that joins a low-space hold posts one more, since it is
news. The end of the hold posts a `System` line. A hold that starts on a
disk-full failure with the same cause as the one last announced, with no
agent created since, is `full_again`: it posts neither, so a disk that stays
full is announced once. The cause is whose disk it was (Archon's store, or
Aether and its agents) and which write failed (SQLite's or another), not the
exact text, which may change from one failure to the next. Low space is
always news: a `full_again` hold whose free space drops below
`limits.min_free_gb` posts one `Blocker` naming both causes, and its end
posts the `System` line. A failed read of the free space leaves the hold as
it was, logs a warning, and shows in `archon status`. `archon status`
(`disk:`), `queue_status` and `archon doctor` show the free space and the
hold. With `worktrees.auto_gc` on, a dispatch pass that finds the hold on
first runs a worktree gc pass (§13.2), at most every 5 minutes, then reads
the free space again, so the space finished runs hold comes back before
anything waits for it. The pass stops after 30 seconds so it does not hold
the dispatch pass up; the worktree it was collecting is finished off the
dispatcher, which that sweep pokes as it ends, and a later pass goes on.

The daemon re-plans on: enqueue, update, cancel and retry of an item, a
stage settled, a gate answered, a run finished, an agent's turn or life
ended (§6.3), caps or pause changed, a gc mark gone, a (re)connect to
Aether, and new spend while a total or daily budget is set. Each of these
pokes the dispatcher itself, so the `dispatcher.tick_secs` (default 15)
timer is a net and not a clock: nothing in the path from enqueue to shipped
waits for it. What it does carry is time-based: stage deadlines, the retry
of a `create_agent` Aether refused as unavailable, and the grace periods of
§6.3.

Spend changes a plan only by reaching `budget_usd_total` or
`budget_usd_daily` (`admission_hold`), so with neither set a usage update is
recorded, checked against its item's budget at once in the fleet pump
(§5.3), and wakes no pass: a pass per usage update of every running agent
bought nothing.

Enqueue and retry refuse an item whose pipeline has a team stage with more
roles than `effective_agent_cap` allows for the item's repository and
pipeline under the caps in force, naming the stage, its role count and the
cap: admitted whole, it would wait forever.

The snapshot needs each queued item's first-stage fan-out and each
repository's `max_per_repo`. A pass reads the config of every repository
with a queued or running item, and the pipeline catalog of every repository
with a queued item (and of items without one), once each and off the async
workers, before it takes the run table. Candidates then share those reads.

### 7.4 Model policy

`[models] allow = ["claude-code/claude-opus-5-5"]` (empty means any model
Aether lists) and `deny = [...]`. A repo config may only **narrow** the
allowlist: the effective list is the intersection. The policy is enforced in
five places:

1. `enqueue`, `update_item` and `retry_item` validate every `ModelChoice` in
   the plan. The error lists the allowed models.
2. `AgentBackend::spawn` re-checks before calling Aether, so a plan persisted
   under an older policy is refused. The item fails with a policy error and a
   `Blocker`.
3. At startup the orchestrator's own model must pass the policy, or `archond`
   refuses to start the orchestrator and reports why.
4. Watchdog: an `agent.created` event under any Archon-owned agent (at any
   depth below a stage agent or the orchestrator, §4.6) whose model or effort
   violates the policy gets `cancel` plus a `Blocker` and a
   `policy_violation` trigger. Below a stage agent the policy is the item's
   repository's effective one. Below the orchestrator it is the global one
   narrowed by each repository whose mirror the helper's scope reads
   (`mirror::reach`): a `workspace_root` or allowed path inside
   `repos/<name>`, or in a git worktree of it, which is what an isolated
   child of a mirror gets, reads that mirror; one that holds the mirrors
   directory reads them all, as does a scope that names no directory or an
   agent Aether no longer knows. So a helper answers to the repository it
   was pointed at, not to the intersection of every repository's policy. A
   helper whose scope leads into a registered repository or the worktrees
   root is cancelled the same way (§4.4).
5. Depth: the watchdog records how many delegations each child is below
   its stage agent or the orchestrator, and cancels one deeper than
   `[limits] max_delegation_depth` (2: two down) with the same `Blocker`
   and trigger. A delegating stage agent's contract states the configured
   depth (§8.6); at 2, its child may delegate once more.

Effort is not restricted by default. `[models] max_effort` optionally caps it.

The orchestrator chooses independently for each stage, following operator
provider preferences and exact model requests first. Pipeline nudges are
advice, not routing rules. Its prompt asks what reasoning and ambiguity remain,
what a wrong result would break and how tests or review would catch it, and
which available model and effort are the cheapest plausible choices. The
highest efforts (`x_high`, `max`) are for a reasoning problem it can name that
inspecting the work left unresolved; that the operator prefers strong models is
not one. Reviewers preferably use a different model from the implementer. A
failed stage prompts reconsideration through `retry_item`'s `model_plan`. The
daemon validates policy; it never ranks models, and never replaces a choice
the orchestrator made.

**Stages the plan leaves out.** A `model_plan` is optional, whole or per
stage, so an enqueue that has nothing to say about a model says nothing.
`knobs::fill_plan` completes the plan before it is stored, in this order:

1. what `learned::default_plan` holds for the repository, pipeline and
   stage;
2. `knobs::default_choice`: the orchestrator's own model when the item's
   policy permits it, else the first model the policy allows, at the effort
   the pipeline's `[nudges] tier` suggests (`fast` low, `balanced` or none
   medium, `strongest` high), lowered to `max_effort` where that caps it;
3. for a stage a knob inserted (§18.1), the first choice of the pipeline's
   first stage.

The filled plan passes the same policy check as a chosen one, and the item
stores it whole, so a run never depends on what the defaults are later. A
policy that allows no model leaves nothing to default to, and the enqueue
is refused naming the stage that has no choice.

`model_evidence` (`archon model-evidence`) computes observations on demand from
existing stage runs and the spend ledger, with optional repo, pipeline, stage
and `since` filters. `since` accepts the same local day syntax as `cost_report`,
but defaults to 30 days including today; attempts are selected by start day.
Each fan-out slot counts as an attempt. Groups are `(stage id, model, effort)`,
sorted by stage then median completed cost, with model/effort tie breakers and
uncompleted groups last within a stage. Each line shows attempts, completed
attempts, median and nearest-rank p90 cost per completed attempt, median minutes
of completed attempts with finish times, and review/test recorded verdict pass
share (unknown verdicts do not pass). Costs use the budget's estimate setting
and latest agent ledger totals, falling back to stored stage cost when there is
no ledger row. Completed n below five is marked as a small sample. Output is
limited to 60 groups and states the omitted count. This evidence recommends
nothing and introduces no persistent cache, background job, price list or storage table.
Stage kinds come from the run's frozen pipeline spec when it is still available,
otherwise from the current catalog. If neither definition resolves (including an
unregistered repository), the kind is unknown: attempts, cost and minutes still
count, but verdicts do not contribute to the pass share. Finished runs normally
discard their frozen specs, so changed current definitions can affect their
classification. Lookups are cached per run and per `(repo, pipeline)` within the
request, with no store lock held across file reads; store failures still propagate.

## 8. Pipelines

### 8.1 Discovery and precedence

Pipelines are discovered from three places, with later sources replacing
earlier ones **by name, as a whole directory**:

1. Built-in: `pipelines/<name>/` in this repository, compiled into the binary
   with `include_str!` through `archon_pipeline::builtin::FILES`.
2. User: `$ARCHON_CONFIG_DIR|~/.config/archon/pipelines/<name>/`.
3. Repository: `<repo>/.archon/pipelines/<name>/`. Only visible to items in
   that repo.

A directory is a pipeline if it contains a `pipeline.toml` entry of any
kind; one that cannot be read (dangling symlink, directory, no permission) is
a broken pipeline, not a missing one. A pipeline that fails to parse or
validate is excluded from the catalog and reported (with path and reason) by
`list_pipelines` and `archon pipelines check`. A broken user override never
silently falls back to the built-in pipeline of the same name. That name
becomes unavailable until the file is fixed. Adding a pipeline is purely a
matter of dropping in a directory. The catalog is re-read at every
`enqueue`, which is cheap.

### 8.2 `pipeline.toml` schema

```toml
[pipeline]
name = "strict-pr-review"            # must equal the directory name
description = "Implement, test, three independent reviews, ship"
version = 1                          # schema version; only 1 exists
workspace = "worktree"               # worktree | repo-readonly | scratch
requires_repo = true                 # false: repo optional (e.g. ask)

# [pipeline.review]                  # optional: one work stage holds the review (below)
# level = "standard"                 # none | standard | strict: what an item gets unless it asks
# stage = "build"                    # a work stage

[pipeline.ship]                      # optional: archond ships, no agent does (§8.7)
stage = "implement"                  # the stage whose result names the commits to ship

[nudges]                             # advice to the orchestrator; never enforced
tier = "strongest"                   # fast | balanced | strongest
independent_reviewers = true         # prefer distinct models/vendors for fan-out
notes = "Security-sensitive changes land here."

[[stage]]
id = "implement"                     # [a-z0-9-]+, unique
kind = "work"                        # propose | work | team | review | test | ship | report
prompt = "implement.md"              # file in this directory
fan_out = 1                          # >1 only for review/test/report kinds; a team's is len(roles)
gate = "none"                        # none | all_pass | majority | any
timeout_minutes = 120
allow_delegation = false             # true lets the stage agent delegate under tools = guarded too (§18.4)
nudge = "Needs strong coding ability."

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 3
gate = "all_pass"
nudge = "Prefer the strongest tier; reviewers independent of the implementer."
on_fail = { goto = "implement", max_loops = 3 }

[[stage]]
id = "build"
kind = "team"                        # one agent and worktree per role (§13.1)
prompt = "build.md"
roles = ["api", "ui"]                # 2 to 8, unique across the pipeline
role_prompts = { ui = "Own the CSS." }   # optional, appended per role
```

Validation rules (load-time errors):

- `name` equals the directory name.
- There are 1 to 32 stages. Stage ids are unique and each `prompt` file
  exists inside the pipeline directory (symlinks may not lead out of it).
- `goto` names an **earlier** stage, and `max_loops` is at most 10.
- `timeout_minutes` is between 1 and 43200 (30 days). It is the stage
  agent's `deadline` as written: a larger value is refused, never clamped.
- `fan_out` is between 1 and 16. A value above 1 is refused for `work` and
  `ship` stages, because they would write to one worktree concurrently.
- Gated stages must be of kind `review`, `test` or `report`, and `review`
  and `test` stages must be gated: with `gate = "none"` their verdict would
  be ignored.
- `propose` and `report` stages need `workspace = "worktree"` or
  `"scratch"`, because they write `.archon/reports/` into the workspace.
- At most one `ship` stage, and it must be last.
- At most one `propose` stage, and it must be first.
- A `team` stage has 2 to 8 `roles`, each 1 to 32 of `[a-z0-9-]` without
  `--`, starting and ending alphanumeric, unique across the pipeline.
  `fan_out` is omitted or equals the number of roles, `role_prompts` names
  only listed roles, there is no gate, and the pipeline uses
  `workspace = "worktree"`. `roles` and `role_prompts` are refused on other
  kinds.
- Every template placeholder in a prompt is known (§8.4), and `team.*`
  appears only in a team stage's prompt.
- `[pipeline.review]`, when present, names a `work` stage and a `level` of
  `none`, `standard` or `strict`; unknown keys are refused.
- `[pipeline.ship]`, when present, needs `workspace = "worktree"` and no
  `ship` stage. Its `stage` is a `work`, `review` or `test` stage with one
  agent, and no `work` or `team` stage follows it; unknown keys are refused.

**A declared review.** A pipeline's review level is otherwise derived from
its gates: three reviewers are `strict`, a gated review `standard`, none
`none` (§18.1). A pipeline whose review happens inside a work stage, by
agents that stage's agent delegates to, has no gate to derive it from, so
it declares the stage that holds the review.

Such a pipeline does not fix a level; the item chooses it. The `review`
knob of an enqueue sets how many independent reviewers the stage owes:

| Level | Reviewers (`archon_pipeline::spec::reviewers`) |
| --- | --- |
| `none` | 0 |
| `standard` | 1 |
| `strict` | 3 (`STRICT_REVIEWERS`), each working alone |

An enqueue that names no level gets the declared `level`, raised to the
ceiling's when the ceiling asks for more (§18.2), and one that names a level
above the ceiling is refused like any other knob. The resolved level is
stored with the item and frozen with the run. A pipeline with gated review
stages keeps the level those stages run: `knobs::review_of` refuses another
and names the pipelines that take one.

The level binds the named stage. Its agent's contract gains a
`## Review on record` section (§8.6), it may delegate whenever the level
asks for a reviewer, and its `complete_task` must carry in `structured`

```json
{"review": {"verdict": "pass", "reviewers": ["<agent id>", "…"]}}
```

naming the agent of each reviewer the pass rests on, a failed one whose
findings were fixed included. Each reviewer's own `complete_task` carries
its verdict on the commits it read:

```json
{"verdict": "pass", "head": "<commit>", "base": "<commit>", "findings": []}
```

A completion without the record, with any other verdict, or with a pass
that names fewer reviewers than the level asks for fails the run at that
stage with what is missing: nothing loops, no later stage starts and
nothing ships. At `none` nothing is owed and the contract says to delegate
no reviewer.

A pass is checked against Aether and the run branch, not taken from the
lead (`executor/review.rs`). When the stage settles, `archond` lists the
lead's descendants once (`list_agents`, the call the owed stop of §6.3
makes) and reads the task result of each named agent (`get_task`). Each
must be a different direct child that completed with a verdict: a
cancelled, failed or running child, a grandchild, or a child that returned
no verdict (a builder) is refused. The verdicts are then placed on the
branch in the run's workspace (`executor/review/coverage.rs`): every `head`
must be a commit the branch holds; a `base` at or before the fork from the
base branch, or none, means the whole change was read; a delta's `base`
must be a head another named reviewer read. A pass counts when it read the
branch head, or when a counted pass read every commit after its head
(`base` at or before it). A fail never counts and must be followed by such
a pass. Counted passes below the level fail the run with the reasons.

The counted agents and the models Aether ran them on are stored as
`review.reviewer_agents` and `review.reviewer_models`; otherwise `archond`
stores `review.unverified` with the reason, and the engine fails the run
at that stage with it. These keys, and the `reviewer_model(s)` and
`reviewer_agent` a 1.0 record held, are Archon's: whatever an agent wrote
under them is removed before the result is stored, so a lead cannot supply
its own finding. Records stored by 1.0 still read as they were. While
Aether cannot be asked the stage stays live and the pump's reconciliation
settles it later. What a reviewer concluded is its own report; Archon binds
that report to the commits it names.

Archon then appends `REVIEW: pass, by <model> (agent <id>), …`, or at
`none` `REVIEW: none, as this item's review level asks`, as the last line
of the stage's summary, from the structured data, so a later stage and the
pull request body carry the record rather than the lead's prose.

A pipeline that still ends in a `ship` stage of its own may also record
`{"review": {"verdict": "skipped", "reason": "…"}}` at `standard`, for that
agent to judge (`REVIEW: skipped: <reason>`). Where archond ships, nothing
judges a skip, so only a verified pass counts.

### 8.3 Stage prompt files

A stage prompt is Markdown with `{{placeholders}}`. It becomes the stage
agent's task `objective`. The stage contract (§8.6) is added separately as
`config.instructions`, so pipeline authors cannot omit it.

### 8.4 Template variables

| Variable | Value |
| --- | --- |
| `{{task.title}}`, `{{task.brief}}` | From the queue item. |
| `{{repo.name}}`, `{{repo.path}}` | Registry entry (empty when no repo). |
| `{{workspace}}` | Absolute path the agent works in. |
| `{{branch}}`, `{{base_branch}}` | Run branch and its base. |
| `{{ship.mode}}` | `pr` or `local`. |
| `{{ship.auto_merge}}` | `true` when the item's `merge` knob is `auto` (§18), else `false`. |
| `{{stage.id}}`, `{{attempt}}`, `{{fan.index}}`, `{{fan.count}}` | Position. |
| `{{run.id}}`, `{{item.id}}` | Archon ids. |
| `{{feedback}}` | Failing verdict summaries and findings from the gate that looped back here, or the operator's feedback on a revision. Empty on the first attempt. Appended to the prompt when the template does not use it. |
| `{{team.role}}`, `{{team.roles}}`, `{{team.branches}}` | Team stages only: this slot's role, every role, and every role's branch for this attempt (comma-separated). `{{workspace}}` is then the role's worktree. |
| `{{previous.<stage-id>.summary}}` | Latest completed summary of an earlier stage, with fan-out summaries joined (a team's labelled by role). |

Substitution is literal. There is no logic and no escaping. `{{{{` produces a
literal `{{`.

### 8.5 Built-in pipelines (v1)

| Name | Stages | Nudges |
| --- | --- | --- |
| `direct` | implement (work, holds the review) | tier = fast. One agent implements a small, low-risk change, runs the affected checks, commits and returns the ship record; the cheapest path to a shipped change. Review level `none` unless the item asks (§8.2); the agent escalates what turns out large or risky. |
| `adaptive` | build (work, `allow_delegation`, holds the review) | tier = balanced, independent_reviewers. For a change where one lead that keeps its context through fixes, or integrates independent parts, saves restarts: it sizes the work, delegates the parts worth splitting, integrates them and has the integrated diff reviewed by as many reviewers as the item's review level asks. Review level `standard` unless the item asks (§8.2). |
| `quick-pr` | implement | tier = fast |
| `default-pr-review` | implement → review (1, all_pass, goto implement ×2) | tier = balanced |
| `bugfix` | reproduce (failing test) → fix → review (1, all_pass, goto fix ×2) | tier = balanced, independent_reviewers. For describable bugs; direct or quick-pr for trivial one-liners, strict-pr-review for security/concurrency bugs. |
| `docs` | write → verify (1, all_pass, goto write ×2) | tier = balanced, independent_reviewers. For documentation: every claim, command and internal link checked against the code. Use `direct` for a typo. |
| `dependency-upgrade` | survey → upgrade → verify (test, all_pass, goto upgrade ×3) | tier = balanced. Version bumps with changelog research and build/tests/audit; use `default-pr-review` for features that add dependencies. |
| `refactor` | baseline/characterize → refactor → verify (test, all_pass, goto refactor ×3) → review (1, all_pass, goto refactor ×2) | tier = balanced, independent_reviewers. Structure-only changes; use default-pr-review for behaviour changes. |
| `strict-pr-review` | implement → test (gate) → review (3, all_pass, goto implement ×3) | tier = strongest, independent_reviewers |
| `team-pr` | plan → build (team: backend, frontend) → e2e (test, all_pass, goto build ×2) → review (1, all_pass, goto build ×2) | tier = balanced, independent_reviewers. For changes that split cleanly into parts. |
| `performance` | measure → optimize → verify (test, all_pass, goto optimize ×3) → review (1, all_pass, goto optimize ×2) | tier = strongest, independent_reviewers. For a specific slow path with a measurable goal; use `default-pr-review` for ordinary changes. |
| `test-coverage` | analyze (work) → write (work) → verify (test, all_pass, goto write ×2) | tier = balanced. Risk-ranked coverage of existing code, at least three passing runs and per-test mutation checks. Bugs become follow-ups; use `bugfix` to fix one. |
| `agent-testing` | plan → exercise (fan_out 2, report) → report | Agents drive the product as a user would and report defects. workspace = worktree |
| `code-audit` | scope (work) → audit (fan_out 3, report) → report | Security, reliability and maintainability lenses produce HTML with high/critical issue drafts. No code changes or ship. workspace = worktree, tier = strongest, independent_reviewers. Use `ask` for one question. |
| `e2e-testing` | setup → e2e (test gate, goto setup ×1) → report | The report stage produces HTML and screenshots under `.archon/reports/` for Lavish. |
| `ask` | answer | workspace = repo-readonly (a disposable snapshot of the repository's `HEAD` commit, §13), requires_repo = false, tier = balanced |
| `errand` | errand (report) | workspace = scratch, requires_repo = false, tier = balanced. A small job answered in its summary and optionally as one self-contained Lavish page. No repository changes or outward side effects; read an attached checkout only, and run code on a throwaway copy in scratch. Other files are temporary and collected after `worktrees.gc_after` (default 7d; §13.2). |
| `design-doc` | research → critique (1, all_pass, goto research ×2) → report (HTML design/RFC) | workspace = worktree, requires_repo = true, tier = strongest, independent_reviewers. Before a large or ambiguous change; no product-code changes or shipping. Use `proposal: lavish` for design and implementation in one item. |

The first twelve change a repository. Each declares `[pipeline.ship]`, so
archond delivers its change once the last stage passed (§8.7) and none has
a ship agent: `team-pr` takes the ship record from `review`, the rest from
their last committing stage. The `propose` and change-report stages the
autonomy knobs insert (§18.1) use the shared prompts in `pipelines/_shared/`.

`direct` and `adaptive` have no review stage: the stage agent delegates the
review, and the item's review level decides how many reviewers that is
(§8.2). They are chosen by naming them (`pipeline = "adaptive"`, or a
`default_pipeline`), never by the `review` knob, which selects
`default-pr-review`, `strict-pr-review` or `quick-pr` when no pipeline is
named. Named, they take any level the ceiling allows (§18.2). At `none`
under `tools = guarded`, `direct`'s agent is a leaf: it cannot delegate, and
its contract carries no delegation section.

### 8.6 Stage contract (appended as instructions)

> You are one stage of an Archon pipeline. Work only in `{{workspace}}`. Do
> not ask the operator questions. If you cannot proceed, call `fail_task` with
> an error starting `BLOCKED:`; if the task does not fit this pipeline, with
> one starting `ESCALATE:`. When done, call `complete_task` with a concise
> `summary`. Review and test stages must include structured
> `{"verdict":"pass"|"fail","findings":[{"severity":"…","title":"…","detail":"…","path":"…"}]}`
> and must end the summary with `VERDICT: PASS` or `VERDICT: FAIL`. Ship
> stages include `{"pr_url":"…"}` or `{"commit":"…"}` in structured data, and
> never amend, rebase, squash or reset the commits they are given: only a
> message that breaks a repository rule may be reworded, with
> `git rev-parse HEAD^{tree}` unchanged and named in the summary.
> Propose stages change nothing, write only
> `.archon/reports/proposal/index.html`, and list it in `structured.reports`.

A team stage's contract names the agent's role and the other roles, keeps
it to its own worktree and role, and asks it to commit on its branch
without pushing: only commits are merged.

A stage agent that can delegate also gets a `## Delegation` section. The
daemon writes it when it builds the spawn request, from what the agent can
actually do (`allow_delegation` on its stage, or the item's `tools = yolo`),
so both routes carry the same rules, and appends it after the contract. It
states:

- a child may delegate once more, passing `leaf: true` for its own
  children: two levels below the stage agent is the limit, and the watchdog
  cancels an agent a third level down (§7.4). That is the default
  `[limits] max_delegation_depth`; at 1 every child is a leaf, and deeper
  limits tell each child its level;
- how the workspace is shared: in a writable workspace a child that edits
  gets `workspace_mode: "isolated"` and the lead brings accepted changes in
  with `integrate_workspace`, while a child that only reads shares the
  lead's workspace; in a `repo-readonly` workspace every child is read-only
  and isolation and integration are not offered;
- the item's effective model policy, in words: the allowed models (or "any
  model Aether lists"), the denied ones and the effort cap. Aether's
  `list_models` shows what is connected, not what is allowed, and the
  watchdog cancels a child outside the policy (§7.4);
- that every child is waited for or cancelled before the stage finishes,
  because Archon cancels whatever still runs when the stage ends (§6.3),
  and that a child that finished cannot be asked again, so a second look
  after a change takes a new child;
- what a child is handed: what the delegating agent already holds, in a
  few lines, and never its conversation, because a child starts cold and
  pays to read whatever it is not told; and for a reviewer the exact base
  and head commits, the changed paths, the checks run with their results
  and the commit they ran on, the known risks and the repository guidance;
- the rules each child's `instructions` must carry. Aether copies nothing of
  a parent's instructions to a delegated child (§17), so the contract does
  not reach one: work only in the workspace it is given and never touch
  other worktrees, other branches or the operator's checkout; do not push;
  never override the git identity; ask the operator nothing; and read only,
  for a reviewer always and for every child of a read-only stage.

The stage a pipeline's declared review names (§8.2) gets a
`## Review on record` section in its contract: how many reviewers the
item's level asks for, the `structured.review` form that names them, that
Archon fails the run without it, and that Archon writes the `REVIEW:` line
itself. The stage `[pipeline.ship]` names gets `## Ship record` (§8.7).
Like the contract, all of this binds by instruction only; what Archon
enforces is named where it is enforced.

`Verdict::extract` reads the structured `verdict` field first, then the last
`VERDICT:` line.

Two error prefixes end a run at once, with no loop, no infra retry and the
stage's live siblings cancelled (`StageOutcome::ends_run`):

- `BLOCKED:` means a person must act. It becomes an item-level `Blocker`
  feed entry.
- `ESCALATE:` means the task does not fit the pipeline: larger or riskier
  than the stage allows for, or better split. The item fails as
  `stage <id>: ESCALATE: <why and what it recommends>` with an ordinary
  `Outcome`, and `item_finished` carries that text to the orchestrator,
  which decides what to do instead. Archon changes nothing itself: no
  other pipeline is started and nothing ships. A `switch_pipeline` or
  `insert_stage` steer queues the failed item again on its branch (§6.6).
  Learning records the run with failure class `escalated` (§19).

### 8.7 Shipping

How a change leaves a run is the repository's ship mode:

- `pr` (default): the run branch is pushed and a pull request opened
  against the base. It is merged only when `ship.auto_merge` is true (the
  item's `merge = auto`), by GitHub once its checks allow.
- `local`: the commits stay on the run branch and nothing is pushed. The
  worktree is kept.

**archond ships.** A pipeline that declares `[pipeline.ship]` has no ship
agent. Its record stage returns, in `structured`, beside whatever else it
returns:

```json
{"commit": "<git rev-parse HEAD>", "tree": "<git rev-parse HEAD^{tree}>",
 "checks": ["…"], "pr": {"title": "…", "body": "…"}}
```

`ShipRecord::read` requires full object ids, a one-line title of at most
256 characters and a non-empty body. The gate checks the record before the
review: a pass without one fails the run at that stage, so no delivery is
ever asked for without it. Stages after the record stage cannot commit
(§8.2), so what they verified is that tree.

When the last stage passes, the driver emits `Ship(ShipPlan)`: the record,
its stage, the run summary, and the pull request body (the record's, then
the `REVIEW:` line, the summaries of the stages that passed after the
record stage under `## Review and verification`, the change report under
`## Change report`, and `Archon item: <id>`; each part clipped so the whole
stays under GitHub's limit). `executor::ship` queues the plan in the run
table and delivers it off every lock:

1. **Wait for the worktree to be quiet.** While the run owes a descendant
   stop (§6.3), an agent a stage delegated to may still write; the delivery
   waits, and the settled stop starts it.
2. **Prove the worktree** (`ship::checks`). It is the worktree git
   registered for the run, not a symlink or a redirected `.git` (§13.1); it
   is on the run branch; no tracked file is modified or staged; `HEAD^{tree}`
   equals the record's `tree`; the branch is ahead of its base; and every
   commit that neither the base nor a remote holds has an author and a
   committer e-mail that the worktree's own git configuration yields
   (`git var`, read with `GIT_*` identity variables removed) or
   `archon@localhost`, under which archond's team merges are made. Any
   miss is a refusal that names the paths or commits. The base is what
   `restack::target_base` reads from the stack (§8.8): the branch of the
   item's first unmerged parent, the repository's base once every parent
   merged, and the run variable `base_branch` for an item that is not stacked.
3. **Push**, `pr` mode only: `git push --porcelain --set-upstream origin
   refs/heads/<branch>:refs/heads/<branch>` with `core.hooksPath=/dev/null`
   and no terminal prompt, so nothing a run configured runs as the daemon.
   It is never forced. A branch `origin` holds with other commits is a
   refusal that says so; a rule or missing access on the remote is
   `BLOCKED:`. Pushing what `origin` has already succeeds.
4. **Find or open the pull request**: `gh pr list --head <branch> --state
   open`. One open pull request is reused, and moved onto the base with
   `gh pr edit --base` when it is for another branch; none makes
   `gh pr create`; several are a refusal. After creating, the list is read
   again and only what GitHub lists counts, so a creation whose reply was
   lost is found, not repeated.
5. **Persist, then finish.** The `ShipOutcome` is written to `kv`
   `ship:<run>`, and a pull request's URL to the record stage run's
   `structured.pr_url`, before the driver acts. `on_shipped` then ends the
   run: `succeeded` with a last summary line `SHIPPED: <url> (commit <sha>)`
   or `SHIPPED: commit <sha> on the run branch`, or
   `failed{stage_id: "ship", error}`.
6. **Auto-merge**, when asked: `gh pr merge --auto` with the first method
   `gh repo view` says the repository allows. The change is delivered
   either way; a refusal is a progress entry.

A refusal is exact and final: the orchestrator reads what to change in the
item's failure and retries. A failure another try may pass (the network,
GitHub, a timeout: 180 s for the push, 60 s per `gh` call) leaves the plan
queued, and the sweep tries again after 15 s, doubling, five times in all
before the run fails with the last error.

Delivery is safe to repeat. A daemon that restarts finds the run `running`
at its last stage; the resumed driver emits the same `Ship`, or, when
`ship:<run>` exists, replays that outcome (`record_ship`) and delivers
nothing. Recovery only queues the plan and the sweep delivers it, so
startup never waits on the network. A crash between the push and the
record repeats steps 2 to 4, which push nothing new and find the pull
request GitHub lists. `prune` deletes `ship:<run>` once its run is terminal.

**A ship stage.** A pipeline may instead end in one `kind = "ship"` stage,
as every built-in did before archond shipped. Its agent delivers with git
and `gh` under the contract of §8.6, and in `pr` mode archond accepts its
completion only once `gh pr list` shows one open pull request for the run
branch, whose URL replaces the one the agent reported. Such a pipeline
declares no `[pipeline.ship]`, and it pays for an agent that reads the run
to do what archond does in seconds.

### 8.8 Stacking

An item queued after others (`depends_on`) starts after their successful
pipeline runs, including delivery; it does not wait for a merge.
`executor/start.rs` forks its branch from the first unmerged
same-repository parent's branch and merges the other parents in. `stacks`
persists each parent's item, branch, SHA and order, and
`Store::stack_base(item)` returns the first unmerged parent's branch and
SHA, or `None`. Cross-repository and scratch dependencies are no stack
parents. The `## Dependencies` section of the contract (§8.6,
`contract::dependency_rules`) names the base the branch was prepared from
and each dependency's item, branch and pull request as bounded JSON.

`restack::target_base` is the base a stacked change is for now: that first
unmerged parent's branch, or the repository's base once every parent
merged. Delivery (§8.7) proves the run against it and opens or moves the
pull request onto it, so a child ships onto its parent's branch.

`executor/restack.rs` reconciles persisted stacks in the coordination worker
(`executor/coordination.rs`), woken when an item finishes and every 60
seconds for changes made outside Archon. GitHub and merge maintenance never
delay the admission of unrelated items. A merged parent has the child's
pull request moved with `gh pr edit --base` to the next unmerged parent or
the repository's base. Commits a parent gains later are merged into the
child's branch, in its registered worktree, and pushed: never rebased or
force-pushed. A conflict keeps the merge and atomically queues one fix item
on the child's branch and worktree, with the conflicting paths as its
brief, under the child's validated pipeline and policy. Git never merges in
the operator's checkout, and every merge commit is made under the
repository's Git identity.

### 8.9 Initiatives

An initiative groups 1 to 16 member items, one per change in a repository,
under a shared contract of at most 2 KiB. `[limits] initiative_members`
(up to 64) and `initiative_contract_bytes` (up to 16 KiB) raise these;
`archon mcp` refuses only what exceeds those ceilings, and `archond` the
configured limits. `enqueue_initiative` validates
every member as an enqueue (`handlers/initiatives.rs::prepare_member`: its
pipeline, model plan and `autonomy` knobs under the repository's ceiling)
before one transaction inserts the initiative and all its items. A repeated
idempotency key returns the original initiative, and different input under
that key is refused. `after` holds indices of earlier members, so the
dependency graph is acyclic; members in one repository stack (§8.8). The
member agents see each sibling's repository, branch, pull request and state
as JSON, with branch names and URLs filtered before they enter another
contract. `update_initiative` appends to the contract (`contract_append`)
or replaces it (`contract`), stores the result and sends the whole new
contract to the members' running leads under durable message keys; the
coordination worker retries what is undelivered. A replacement with the
same text changes nothing.

An initiative moves `open → shipping → done`, or to `failed` or
`cancelled`. The first delivered pull request starts `shipping`, and every
member passing completes it. A member that fails or is cancelled fails the
initiative and holds the queued members. Cancelling stops the unfinished
members and keeps successful work and pull requests. A retry needs a failed
or cancelled initiative with no active member, queues its failed, cancelled
and blocked members again together on their branches, keeps the successful
ones and starts a new notification cycle. A member's own retry is held
while its initiative is failed or cancelled.

`executor/initiative_ship.rs::record` is called by delivery (§8.7) for a
member. It stores the verified pull request URL and replaces a marked
"Part of …" section in each member's pull request body with links to its
siblings. An atomic initiative's members are forced to `merge = manual` (a
member asking for `auto` is refused at enqueue), their pull requests are
opened with `gh pr create --draft`, and once every member passed the
coordination worker readies each with `gh pr ready`. Every write to GitHub
is idempotent and replayed from stored state after a restart.

A member's finish posts its outcome to the feed and wakes nobody (§5.4).
A terminal initiative pushes one `initiative_finished` trigger per cycle,
whose durable receipt is committed in the same transaction as the trigger
outbox, so a failed member reaches the orchestrator through its
initiative.

## 9. Configuration

`$ARCHON_CONFIG_DIR|~/.config/archon/config.toml`. Every field is optional,
and the example below shows the defaults. Unknown keys are an error, so typos
cannot silently disable limits. `archon setup` writes the file with every
key commented out when there is none, and never touches one that exists.
No other file in that directory configures Archon: an `archon.toml` from
before 1.0 is not read, `archon doctor` warns about it, and `archond` posts
one notice for it as it starts (`kv.legacy_config_told`), naming the file
that is read.

```toml
[aether]
mode = "dedicated"             # Archon's own aetherd (§4.7); "shared" = the operator's
# Shared mode only (refused in dedicated mode):
# socket = "/run/user/1000/aether/aetherd.sock"  # unset: Aether's own resolution (§4.1)
# address = "127.0.0.1:7777"   # TCP instead of the socket
token_env = "AETHER_TOKEN"     # env var holding the TCP token

[orchestrator]
model = "claude-code/claude-opus-5-5"
effort = "high"
name = "Archon"
# instructions_file = "~/.config/archon/orchestrator.md"  # appended to the built-in prompt
trigger_coalesce_ms = 2000
wake_on_success = true         # false: a routine success waits for its next turn (§5.4)

[models]
allow = []                     # e.g. ["claude-code/claude-opus-5-5"]; [] = any
deny = []
# max_effort = "high"          # unset: no cap; else low|medium|high|x_high|max

[limits]
max_parallel_agents = 8
max_per_repo = 0
max_active_runs = 0
budget_usd_per_item = 0.0
budget_usd_daily = 0.0
budget_usd_total = 0.0
count_estimated_cost = true
infra_retries = 1
min_free_gb = 10               # hold dispatch below this much free disk; 0 = off
max_delegation_depth = 2       # delegations below a stage agent or the orchestrator; 1..=4
max_agent_messages = 20        # message_orchestrator calls per agent, lifetime; 1..=200
initiative_members = 16        # 1..=64
initiative_contract_bytes = 2048  # 1..=16384
[limits.per_pipeline]          # name = max running stage agents
# strict-pr-review = 3

[dispatcher]
tick_secs = 15

[chat]
show_progress = false          # the /progress toggle's initial value
history = 30                   # feed entries replayed when chat opens

[worktrees]
# root = "/srv/archon/worktrees"  # unset: <data>/worktrees
branch_prefix = "archon/"
keep_on_success = false
auto_gc = true                 # archond's own worktree gc passes (§13.2)
gc_after = "7d"                # remove a finished run's worktrees after this
cache_after = "1h"             # delete their build caches after this
cache_dirs = []                # e.g. ["target", "node_modules"]; [] = all git ignores

[lavish]
enabled = "auto"               # auto | on | off
binary = "lavish-axi"

[github]
binary = "gh"                  # posts github_reply comments (docs/github-watcher.md)

[autonomy.default]             # knobs an item gets unless it names others (§18)
review = "standard"            # strict | standard | none
proposal = "off"               # lavish | off
report = "off"                 # before-pr | with-pr | off
merge = "manual"               # manual | auto
tools = "guarded"              # guarded | yolo; also the orchestrator's (§18.4)

[autonomy.max]                 # the ceiling; the daemon refuses anything above it
review = "none"
proposal = "off"
report = "off"
merge = "manual"               # "auto" lets items merge their own PRs
tools = "yolo"

[learning]                     # §19
enabled = true                 # false: no outcomes, learned defaults, lessons or distillation
distill_every = 20             # new outcomes of a repository per distillation errand
explore_rate = 0.1             # share of items that try a cheaper choice; 0..0.1
```

Per-repository `<repo>/.archon/config.toml`:

```toml
[repo]
ship = "pr"                    # default for `archon repo add` when --ship is omitted
base_branch = "main"
default_pipeline = "default-pr-review"

[limits]
max_per_repo = 2               # may only lower the global value

[models]
allow = ["claude-code/claude-opus-5-5"]   # intersected with the global allowlist

[pipelines]
disabled = ["quick-pr"]

[autonomy.max]                 # may only lower the ceiling; [autonomy.default] likewise
tools = "guarded"
```

Precedence for repository settings: registry flags from `archon repo add` or
`archon repo set`, then `.archon/config.toml`, then global defaults.
**Only the operator can raise an autonomy ceiling**, through the global
`[autonomy] max` or the registry (`archon repo set --autonomy-max`). A
repository file cannot opt itself into merging or anything else (§18.2).

Paths: `ARCHON_CONFIG_DIR` (`config.toml`, `pipelines/`, and in dedicated
mode the instance's `aether/`), `ARCHON_DATA_DIR` (store `archon.db`,
`worktrees/`, `orchestrator/`, `reports/`, `logs/`, and in dedicated mode
`aether/` and `aetherd-start.lock`) and `ARCHON_SOCKET`
(`$XDG_RUNTIME_DIR/archon/archond.sock`; the instance's `aetherd.sock` sits
beside it).

Paths in `config.toml` (`[aether] socket`, `[orchestrator]
instructions_file`, `[worktrees] root`) are resolved when the file is read,
never against a process's working directory: a leading `~/` expands to the
home directory, and any other relative path is relative to the directory
holding `config.toml`. `~user/` is refused. `archon daemon start` and
autostart run the detached archond in the data directory, and hand it
`ARCHON_CONFIG_DIR`, `ARCHON_DATA_DIR` and `ARCHON_SOCKET` as absolute
paths resolved where the command ran, so a relative override keeps its
meaning there. A foreground `archon daemon run` stays in the directory it
was started in.

## 10. The Archon daemon protocol

This protocol mirrors Aether's so both are debugged the same way. It is
defined in `archon_core::protocol` and served by `archon-daemon`.

- Unix socket at mode 0600, NDJSON, 16 MiB frame cap.
- Client frame: `{"req":N, "cmd":"<snake_case>", ...}`. The first frame must
  be `hello {protocol_version: 6, client: "archon-cli"|"archon-mcp"|"archon-bench"}`.
- Server frames: `{"frame":"reply","id":N,"result":{"Ok":{"reply":…,"data":…}}|{"Err":{"code":…,"message":…}}}`
  and `{"frame":"feed","sub":S,"item":FeedItem}`.
- `ClientKind` is recorded per connection. The socket's file mode is the
  security boundary. `ClientKind` guards against mistakes, not attacks.
  Every `Request::is_operator_only()` request (`Say`, `Answer`,
  `RepoUpdate`, a `RepoAdd` with `auto_merge`, a hard pause, ...) is refused
  for `archon-mcp`. `RecordChatAnswer` is not one of them: it is the
  orchestrator's, checked by caller identity, and cannot answer a gate (§5.2).
- `ForAgent {caller, request}` carries a tool call `archon mcp` forwards
  for the agent Aether named as making it. `archond` authorizes the caller
  and applies the inner request as one step (§11). A request from
  `archon-mcp` that changes Archon and comes without this wrapper is an
  unnamed call. It wraps no operator-only request and no second `ForAgent`.
- The CLI takes an item id or its start. A whole id is sent as it is; a
  start goes to `ResolveItem {prefix}` first, which `archond` answers from
  one indexed range query: the one item, `not_found`, or `invalid_argument`
  naming the items the start matches. `ListItemSummaries` is `ListItems`
  with each item's live stage, spend and newest `pr_url`, for
  `archon queue list`.
- A clearable setting in a patch (`RepoPatch.base_branch` and
  `default_pipeline`) is a `Clearable`: `"keep"` (the default when absent),
  `"clear"`, or `{"set":value}`. `archon repo set --base ""` or
  `--pipeline ""` sends `"clear"`.

The command set is `archon_core::protocol::Request`, and every variant is
documented there. Reply kinds are `archon_core::protocol::Reply`. Error codes
are `ErrorCode`. A store failure (`ArchonError::Store`, or `DiskFull` when
SQLite reports a full disk) is `internal` on the wire, and its message says
`store: disk full: ...` in the second case. Inside `archond`,
`ArchonError::is_store()` covers both: every worker stops on one (§16).

## 11. MCP tool surface

`archon mcp` implements MCP over stdio (JSON-RPC 2.0, protocol revision
`2025-06-18`): `initialize`, `notifications/initialized`, `tools/list`,
`tools/call` and `ping`. Each tool maps to one `Request`. Results are returned
as `content: [{type:"text", text:<compact human summary>}]` plus
`structuredContent: <Reply data>`. Daemon errors become `isError: true`
results whose text is the daemon's message. The model sees the error and can
correct itself.

`initialize`, `ping` and `tools/list` are answered as soon as their line is
read. Tool calls run concurrently, at most 64 at once; one more gets a busy
tool error at once and is not started. Each call must finish within 15 s of
the moment its line was read, daemon autostart included, so a slow call
cannot push the calls behind it past Aether's 20 s MCP timeout. Responses
carry their request's id and may leave in a different order than the
requests. `notifications/cancelled` is only logged: the call still ends
within its 15 s. When stdin closes, the calls in flight are answered before
the server exits.

Every message, in either direction, is one line of at most 4 MiB, because
Aether treats a longer one as a dead server. Request ids may be at most 256
bytes serialized; a longer id is refused (`-32600`, id `null`). A JSON-RPC
error message is cut at 4 KiB and a tool error's text at 16 KiB, ending in
`… [truncated from N bytes]`, so quoted input cannot overflow a response. A
tool result that would not fit becomes a tool error suggesting `limit`, and
any other response that would not fit becomes error `-32603` under the same
id.

Every tool that changes Archon acts for the orchestrator, and the agents it
delegates to inherit its tool exposure (§4.4). So a mutating tool other than
`message_orchestrator` answers only the current orchestrator, and `archond`
is the one that decides:

- `archon mcp` wraps such a call as `ForAgent { caller, request }` (§10),
  with the agent Aether named on the call (`send_caller_meta = true`, §5.5),
  and sends it bare when Aether named none. It asks nobody first: there is
  one request, inside the call's 15 s.
- `archond` compares the caller with `kv.orchestrator_agent` while it holds
  the supervisor's authority lock for reading, and keeps holding it until
  the request has been applied. Every path that puts another agent in charge
  takes that lock for writing, so the orchestrator is not replaced between
  the check and the change, and a call does not land for an agent that has
  just stopped being the orchestrator. The answer comes from the store; no
  Aether round trip is made.
- Anyone else is refused with what Archon's records say it is and what to
  do instead: a replaced orchestrator (stop), a stage agent
  (`message_orchestrator`), a delegated agent of an item or of the
  orchestrator (finish and report to its parent), or an agent Archon does
  not know. That last one covers the moment after Aether restarted the
  orchestrator into a new agent and before `archond` adopted it: the text
  says to call once more in a few seconds.
- Read-only tools answer any caller.

**When Aether names no caller.** A registration written before Archon
required `send_caller_meta`, or an `aetherd` that started before the line
was added, sends calls without it. Refusing them all would lock an operator
out of an orchestrator that an upgrade had just broken; forwarding them all
would let a delegated agent change Archon. So `archond` forwards an unnamed
mutating call only when the orchestrator in charge is one it has seen to be
unable to delegate natively (`kv.orchestrator_sole_caller`, recorded from the
agent's own config when it takes charge): then no agent of Archon's can
be the caller but that orchestrator. In shared mode another agent of the
operator's own daemon also sees these tools and is not told apart until
callers are named, as before this check existed. Otherwise the call is
refused with the fix. The first unnamed call
sets `kv.mcp_callers_unnamed` and posts one `Blocker`; while it is set,
every new orchestrator generation is created without `delegate`, with a
note in its instructions saying why. A named call from the orchestrator,
a read as much as a change, clears it, says so in the feed, and the next
generation delegates again: only a registration that names callers sends
one, so the key follows the registration in force and not the last change
the orchestrator happened to make. An unnamed call after that is judged as
the first was and sets the key again.
An orchestrator created by a release before delegation has no `delegate`
either, so an upgraded install keeps working before anything is repaired.

In dedicated mode the registration is Archon's own file and is repaired
without the operator (§4.7). In shared mode Archon never edits the
operator's Aether config: `archon doctor` fails the registration check with
the line to add, and the notice above says to restart that `aetherd`.

This guards against an agent queueing or cancelling work it was not asked
to; it is not a security boundary, since any process of the operator's user
can reach the socket (§5.5).

| Tool | Arguments | Request |
| --- | --- | --- |
| `list_pipelines` | `{repo?}` | `ListPipelines` |
| `list_repos` | `{}` | `RepoList` |
| `model_policy` | `{repo?}` | `ModelPolicy` |
| `register_repo` | `{path, name?, ship?, base_branch?, default_pipeline?}` | `RepoAdd` (never raises a ceiling) |
| `enqueue` | `{repo?, pipeline?, title, brief, priority?, depends_on?, tags?, model_plan?, model_reasons?, budget_usd?, idempotency_key?, autonomy?, origin?}` | `Enqueue` |
| `list_queue` | `{states?, repo?, pipeline?, tag?, limit?}` | `ListItems` |
| `get_item` | `{item}` | `GetItem` (stage summaries and errors clipped at 1500 characters, naming `get_stage`, and the run unless it is the latest) |
| `get_stage` | `{item, run?, stage?, attempt?, slot?, offset?, limit?}` | `GetStage` |
| `update_item` | `{item, priority?, model_plan?, model_reasons?, tags?, add_depends_on?, budget_usd?, brief_append?}` | `UpdateItem` (not after success; on a running item `model_plan` and `brief_append` are steered into its run, §6.6; neither while dispatching) |
| `steer_item` | `{item, note?, stage?, model?, effort?, budget_usd?, extend_minutes?, abort_stage?, then?: retry\|skip\|goto, goto?, insert_stage?: {stage, from?, after?}, switch_pipeline?}` | `SteerItem` (§6.6; at least one change) |
| `enqueue_initiative` | `{title, contract, atomic?, members: [{repo, brief, title?, pipeline?, model_plan?, autonomy?, after?}], idempotency_key?}` | `EnqueueInitiative` (§8.9; members validated as enqueues, created together) |
| `update_initiative` | `{initiative, contract_append \| contract}` | `UpdateInitiative` (§8.9) |
| `cancel_item` | `{item, reason?}` | `CancelItem` |
| `retry_item` | `{item, model_plan?, model_reasons?, fresh?, brief_append?}` | `RetryItem` (§13: `fresh` starts again from the base branch) |
| `queue_status` | `{}` | `Status` |
| `model_evidence` | `{repo?, pipeline?, stage?, since?}` | `ModelEvidence` (30 local days by default; stage attempt observations, at most 60 groups) |
| `cost_report` | `{since?, until?, by?: day\|repo\|pipeline\|model\|item\|stage}` | `CostReport` (§5.3) |
| `pause` / `resume` | `{scope: "global"\|"repo"\|"pipeline", name?}` | `Pause` / `Resume` |
| `notify` | `{kind: question\|decision\|blocker\|outcome\|progress, text, item?}` | `Notify` |
| `ask_operator` | `{text, item?, options?}` | `AskOperator` (Aether's caller identity, when sent, is stored as the question's asker; §5.2) |
| `record_chat_answer` | `{question, answer}` | `RecordChatAnswer` (the orchestrator only, and only `ask_operator` questions it asked itself; §5.2) |
| `publish_report` | `{title, path (.html or .htm), item?}` | `PublishReport` |
| `autonomy_policy` | `{repo?}` | `Autonomy` |
| `set_directive` | `{text, expires_in_hours?}` | `SetDirective` |
| `clear_directive` | `{directive?, all?}` | `ClearDirective` |
| `github_reply` | `{repository, number, body}` | `GithubReply` (Archon's watched repositories only; see `docs/github-watcher.md`) |
| `record_lesson` | `{kind: routing\|practice\|pitfall\|pipeline, text, repo?, pipeline?, item?}` | `RecordLesson` (§19; active at once, advice only) |

`get_stage` reads the item's latest run, or the one `run` names; attempts
restart at 1 in each run. Without `stage` it lists every
stage run: stage, attempt, slot, model, state, verdict and cost. With
`stage` it shows each matching stage run's error, whole summary and
`structured` result pretty-printed as JSON: the latest attempt unless
`attempt` names one, every fan-out slot unless `slot` names one. That output
is never clipped. It comes in pages of `limit` characters (default 20000, at
most 100000) from `offset`, and a page that is not the last ends by naming
the call for the next, run included. `attempt`, `slot`, `offset` and `limit`
need `stage`. An unknown item, a run of another item, a stage that has not
run, or a missing attempt or slot is `NotFound`, naming what exists. A result
over the MCP message limit is refused with a hint for the tool called: for
`get_stage`, a smaller `limit`, `offset` and `slot`.

`model_plan` is `{"<stage-id>": [{"model":"provider/model","effort":"high"}, …]}`.
It is optional at `enqueue`, whole or per stage: Archon fills the stages it
leaves out (§7.4), and the item shows the filled plan.
`model_reasons` is an optional `{"<stage-id>": "one-line reason"}` object,
with at most 300 Unicode characters per reason. Unknown stage ids are Invalid
and list valid ids. It belongs to the item, not `ModelChoice` or executed stage
choices. `enqueue`, `update_item` and `retry_item` accept it; provided reasons
replace the recorded object. Replacing `model_plan` without reasons clears old
reasons; omitting both preserves them. Old items and clients have no reasons.
`get_item` and the CLI item view show supplied reasons beside plan choices.
`autonomy` is `{"review"?, "proposal"?, "report"?, "merge"?, "tools"?}` (§18.1).
`origin` is the GitHub thread `{"repository":"owner/name","number":N}` a
request came from; the item's `item_finished` event carries it back.
`model` accepts the `provider/model` string or Aether's
`{provider, model}` object, and `effort` accepts `xhigh` for `x_high`. The
advertised schemas say exactly this: a test validates accepted arguments
against each schema, and checks that arguments the schema refuses are refused.

## 12. Operator chat and filtering

`archon chat` opens a line-oriented chat. It is not a TUI, and it works over
SSH and in pipes; it is also what a bare `archon` opens where the TUI cannot
run (§4.7). The TUI shows the orchestrator's transcript, not this feed: what
`archond` posts itself (gate questions, stage reports, `System` entries)
appears there only as far as the orchestrator restates it; see
[the guide](guide.md#what-the-terminal-ui-shows-and-what-it-does-not).

- On open, it prints the last `chat.history` **visible** feed items, then
  subscribes with `SubscribeFeed{after, verbose}`.
- Input lines are sent with `Say`. Slash commands: `/progress` (toggle
  verbose), `/a <n> <text>` or `/answer <n> <text>` (a bare `/a <text>`
  answers the newest open question), `/status`, `/queue [state …]`,
  `/item <id>`, `/pause`, `/resume`, `/help`, `/quit`. An operator gate's
  question (§18.3) reads `/a 3 yes`, `/a 3 <feedback>` or `/a 3 reject`.
  Question numbers are archond's permanent ones (§5.2).
- Text that agents or repositories wrote (feed entries, questions and their
  options, item titles and briefs, errors, gate notes) is printed without
  terminal controls: C0/C1 control characters (ESC among them) and bidi
  overrides are removed, and one-line fields have their line breaks turned into
  spaces. `--json` output is unchanged.

A `FeedEntry` is created only by:

| Source | Feed entry | Shown by default |
| --- | --- | --- |
| Operator `Say` | `Operator` | yes (echo) |
| Final assistant text of an orchestrator turn that consumed an operator message | `Orchestrator` | yes |
| `notify(kind)` | `Notification` | yes except `progress` |
| `ask_operator`, `question.asked` (orchestrator or stage agent) | `Question` | yes |
| An answer | `Answer`, with `via` when it did not come through Archon's own surfaces: `chat_command` (the operator's `/answer` in the orchestrator's Aether chat) or `chat_reply` (recorded by the orchestrator with `record_chat_answer`); §5.2 | yes |
| Item terminal state | `Notification{Blocker}` if operator action is needed, otherwise `Notification{Outcome}` | yes |
| Blocked item, budget hit, policy violation, `BLOCKED:` stage | `Notification{Blocker}` | yes |
| Daemon lifecycle (orchestrator restarted, Aether reconnected) | `System` | yes |

**Reply detection** follows only the orchestrator's ordered event stream, so
it cannot race `Say` or a lagging pump. An `inbox.delivered` message from the
`user` actor is pending until an `inbox.consumed` (at turn start or steered
mid-turn) covers its inbox seq. From then on `archond` keeps the last
assistant text, and on `agent.turn_completed` it emits that text. A turn that
ends `failed` or `cancelled` emits nothing; the revived agent answers. A
turn that ends waiting for something other than its inbox (`agent.waiting`
with a condition, as when the orchestrator waits for an agent it delegated
to) emits what it said and leaves the answer open: the last text of the
turn that the wait's end wakes is emitted too, so a delegated result is
shown as the reply. The answer closes at the first turn that ends any other
way. While it is open, a turn woken by Archon news is shown as well, which
is the price of not dropping the answer. The
tracker is persisted in kv `chat_reply_state` with the last event seq it
applied, so neither a restart nor a replay loses or repeats a reply.
Assistant text from other trigger-woken turns is never shown. Tool calls, thinking
and intermediate text are never shown. `archon log` and Aether's TUI show
everything. `archon log --item <id>` keeps the entries about one item (its
notices, questions and their answers, reports), `--kind` keeps some kinds
(`operator`, `orchestrator`, `question`, `answer`, `decision`, `blocker`,
`outcome`, `progress`, `report`, `system`), paging back through
`FeedHistory` until `--limit` entries match, and `--follow` then
subscribes after the newest entry read and prints each new match (one JSON
object per line under `--json`).

## 13. Repositories and worktrees

- `archon repo add <path> [--name N] [--ship pr|local]
  [--base B] [--pipeline P] [--auto-merge]`. The path is canonicalized and
  must be a git top-level. The name defaults to the directory name and must
  pass `Repo::valid_name`. `--auto-merge` sets the repository's autonomy
  ceiling to `merge=auto`; `archon repo set --autonomy-max <knob=value,…>`
  replaces that ceiling later (§18.2).
- Worktree per pipeline run:
  `<worktrees.root>/<repo>/<item-id>` on branch
  `<branch_prefix><item-short>-<slug(title)>`, created with
  `git -C <repo> worktree add -b <branch> <path> <base>`. The base is
  `repo.base_branch`, or the repo's current `HEAD` commit.
  **Uncommitted changes in the operator's checkout are not included**, unlike
  Aether's isolated-delegation snapshot.
- `workspace = "repo-readonly"` runs the stage in a snapshot, never in the
  operator's checkout: a detached worktree of the repository's current
  `HEAD` commit at `<worktrees.root>/<repo>/<item-id>/snapshot`, added with
  `git -C <repo> worktree add --detach`. Like any worktree, it leaves out
  uncommitted changes. Nothing in it is kept. When the run ends, whether it
  succeeded, failed or was cancelled, the daemon proves it is the worktree
  git registered there (never a symlink, nor reached through a symlinked
  item directory), removes it with `git worktree remove --force --force`,
  and removes the item directory once empty. A start that gives up after
  adding it removes it too, and a snapshot an earlier run of the item left
  behind is removed the same way before the next run adds a fresh one, or
  a `worktree` run adds its worktree in the item directory. The prompt still forbids edits, which is not
  enforced (§17), but what the agent writes lands in the snapshot. Without
  a repository the stage runs in scratch. `scratch` runs in
  `<data>/scratch/<item-id>`.
- Retries reuse the run's worktree when it still exists (the fix loop wants
  prior work). An item retry creates a fresh run but keeps the item's
  worktree and branch, because the worktree path is keyed by item: the new
  run starts from the prior attempt's work, committed or not. If `archon gc`
  removed the worktree, it is added again on the kept branch.
- A fresh retry (`retry_item {fresh: true}`, `archon queue retry --fresh`)
  starts the item again from the base branch's current head instead. Nothing
  is committed or discarded: the old run worktree (`<item>`, or a team run's
  `<item>/main`) is moved aside unentered, as a role's is (§13.1), to
  `<worktrees>/<repo>/.set-aside/<item-id>-<n>` (a team run's to
  `<item>/.set-aside/main-<n>`), with its git directory under
  `<common>/archon-set-aside/`, and the progress feed says where. The next
  run forks a new branch, `<old branch>-fresh<n>`, where `n` counts the
  item's fresh retries (`QueueItem.fresh_retries`); the old branch, its
  commits, the remote and any pull request open from it are left as they
  are. Shipping never forces a push, so the fresh run opens a new pull request
  and the old one should be closed by the orchestrator or the operator. A
  plain retry after a fresh one stays on the fresh branch. The retry is
  refused, and nothing moves, when the new branch already exists or the
  pipeline has no repo worktree (`scratch`, `repo-readonly`), and, with
  `unavailable`, while a worktree gc pass is collecting the item. Worktree
  gc (§13.2) collects what a fresh retry set aside with the item's
  worktree: a team run's `main` with the team root, a flat item's
  `.set-aside/<item-id>-<n>` and its git directory on the run worktree's
  cutoff, even when a shipped run already removed that worktree. That is
  `worktrees.gc_after` (default 7d) after the item's last run, with no check
  for uncommitted changes, which `archon queue retry --fresh`, `retry_item`
  and the progress note say.
- A brief amendment (`brief_append` on `retry_item` or `update_item`, a
  `note` on `steer_item`, `archon queue retry --append`) is appended to the
  brief under a dated `## Amendment (<UTC time>)` heading, so every later
  stage prompt carries it, and a progress note records it against the item.
  A running item's live agents are sent it (§6.6); only an item that is
  dispatching is refused.
- Cleanup: after `succeeded` with a completed ship-kind stage in the run's
  frozen pipeline, repository ship mode `pr`, and when
  `keep_on_success = false`, the worktree's build caches are deleted
  (`git clean -fdX`, or only within `cache_dirs`), then the worktree is
  removed with `git worktree remove`, never forced, and the branch is
  kept. A removal that fails, for a stray untracked file say, is reported
  in the feed with the space the deleted caches freed. Successful runs that
  shipped nothing, failed runs and cancelled runs keep their workspace for
  inspection until normal gc retention. Read-only snapshots remain disposable.
  Worktree gc (§13.2) deletes
  what finished runs keep: their caches after an hour, the worktrees after
  a week.
- A start that gives up after it added a worktree (the pipeline refuses the
  plan, the item was cancelled meanwhile, or its run cannot be inserted)
  removes that worktree, since no run row leads `archon gc` to it. A
  worktree the start reused from an earlier run is kept, and a removal that
  fails is reported in the feed.

### 13.1 Team stage layout

A pipeline with a `team` stage lays each run out one level deeper, so the
role worktrees sit beside the run worktree and never inside it:

```
<worktrees.root>/<repo>/<item-id>/            run node scope (PipelineRun.team_root)
  main/                                       run worktree, run branch
  <stage>-<role>/                             one per role, <run branch>--<role>-<run8>-a<attempt>
```

`<run8>` is the tail of the run id. Every run of an item shares the run
branch and the `<item-id>` directory, and its attempts start at 1 again, so
the run id keeps a retried item off an earlier run's worker branches.

Pipelines without a team stage keep the flat `<item-id>` worktree, and a
team pipeline refuses an `<item-id>` that holds one, and the other way
round; so does an `<item-id>` that still holds a role's worktree after
the run worktree was removed.

A role controls its worktree, `.git` file included, and a work stage the
run worktree. Before the daemon runs git in either
(`worktree::team::identity`), it proves the directory is the worktree git
registered there, so its commits, switches and merges never land in the
operator's checkout or another worktree:

- the directory is not a symlink, and its `.git` is a plain gitfile, not a
  symlink or a directory;
- git's top level there is the directory itself, so git never walks up
  into an enclosing repository;
- `--git-common-dir` is the common git directory of the item's registered
  repository, so git config a role planted elsewhere is never read;
- `--git-dir` is an administrative directory `<common>/worktrees/<name>`,
  never the common directory itself (a gitfile naming the operator's
  `.git`), and its `gitdir` file, absolute or relative, names this
  directory's `.git` (not `main`'s or another role's).

The top level and both git directories come from one `git rev-parse
--path-format=absolute --show-toplevel --git-common-dir --git-dir`; output
other than three lines (a path with a newline) is refused.

A check that fails is an error: a role's slot fails, or the merge fails
the run, before any git that writes runs.

`status`, `diff` and `add` recurse into populated submodules and run the
filters their git config names, so a repository a role made inside its
worktree and registered as a gitlink could run any command as the daemon.
Before any such git (`worktree::team::submodules`), the daemon lists the
gitlinks in the index (`ls-files --stage`, which enters nothing), at every
nesting level up to 8, and refuses a populated one whose path is a
symlink, whose `.git` is a directory or symlink, whose gitfile names a git
directory outside `<common>/modules/` and `<common>/worktrees/<name>/modules/`
(where `git submodule` keeps them), or from which git finds another work
tree. In a role's worktree at the merge this is `Refused`, naming the
submodules. At the role's next attempt such a worktree is not switched,
since saving what it left would enter them: it is renamed, unentered, to
`<item-id>/.set-aside/<stage>-<role>-<n>` (the first free `<n>`), and
its administrative directory `<common>/worktrees/<name>` is renamed to
`<common>/archon-set-aside/<name>-<n>`, out of git's list of worktrees,
rather than removed: it holds the worktree's index and reflogs and, in
`modules/`, the git directories of submodules the role added with `git
submodule add`, with any commit made only inside them. Git gives the
fresh worktree the same administrative name, so nothing kept may name
it: the gitfiles of the worktree's submodules, the ones git must not
enter included, that named a git directory in it are pointed at where
each is kept (so git works in them as before); every submodule git
directory under the moved `modules/`, at any depth, has its
`core.worktree` pointed at the same place in the kept worktree, or
removed when it named anything outside the worktree; and the worktree's
own gitfile is removed. The kept worktree gets no `.git`, deliberately:
one would make any git run there apply the config and filters a role
planted. The notice gives the operator `git --git-dir=<kept git dir>
--work-tree=<kept worktree> status` to run once they have checked its
submodules' config. `.set-aside` and `archon-set-aside` must be real
directories: when either is a symlink or a file, nothing is moved and
the slot fails. The administrative directory is moved first and
the gitfiles changed before the worktree is moved: a crash in between
leaves a directory at the role's path git cannot use, and the next
attempt fails with an error until the operator moves it, while the
daemon's log already names both destinations. A lock (`git worktree
lock`) moves with it. A fresh worktree is added in its place, and the
move is posted to the run's progress. Every file stays for the operator
to inspect and remove; its commits stay on its branch, or, from a
detached `HEAD`, under `refs/archon/saved/<commit>`. Objects only the
moved index or a per-worktree ref reach are no longer kept from `git
gc`. `archon gc` removes both once the item is terminal and older than
its cutoff (§13.2). An item directory holding
only `.set-aside` still counts as a team run's. A worktree whose `HEAD`
names a reference with no commit (a role ran `git symbolic-ref`) is moved
aside the same way, since what it left belongs to no commit and cannot be
saved; the notice says so. When adding the fresh worktree then fails,
the error still carries the notice, so the operator learns where the old
one went. An infra
retry of the same attempt keeps the worktree as it is, as no git that
enters a submodule runs then. In the run worktree it is an error as
above. Unmerged files are read from the index (`ls-files --unmerged`),
never with `diff`.

The repository's common git directory is the operator's and trusted:
git reads its config for every command, the operator's own included. A
role can still write config git reads there, as `git config` does from
its worktree; so can it for a submodule it added with `git submodule
add`, whose git directory and config are under
`<common>/worktrees/<name>/modules/` (`git -C sub config filter.x.clean
<cmd>`, run when `status` enters the submodule). The submodule check
keeps out repositories git never registered, not config written through
git: a role's git config is trusted like the repository's, and a role
that must not be is to be isolated from the repository by other means.

`HEAD` is read with `symbolic-ref`, never `rev-parse --abbrev-ref`, which
says `heads/<name>` when a tag has the branch's name; a `HEAD` pointed at
a tag or another ref counts as a branch not the role's. Roles fork from
`refs/heads/<run branch>`, never from a tag of that name.

Every git the daemon runs has
`core.fsmonitor=false`, and in a team's worktrees also
`core.hooksPath=/dev/null`: `worktree add`, `switch`, `update-ref`, the
`--quit`s and the daemon's commits and merges run no repository hook,
which a role could have committed where `core.hooksPath` looks. Paths are
read NUL-terminated (`-z`), so names with spaces at either end or a
newline are listed exactly; a name that is not UTF-8 is shown lossily
and never fails the merge.

- **Workers.** Before a slot's agent is issued, the executor makes the
  role's worktree ready (`worktree::team::ensure_worker`). An infra retry
  finds the attempt's branch and reuses the worktree as it is. An existing
  branch not checked out there is reused only when the run branch is an
  ancestor of it (`merge-base --is-ancestor`); otherwise it is not this
  run's and the slot fails. Otherwise
  the branch is new and forks from the run branch's `HEAD`: the worktree is
  added, or an existing one (an earlier attempt's, or an earlier run's)
  switches to it, unless it holds submodules git must not enter or its
  `HEAD` names no commit, when it is moved aside as above. A role's
  worktree deleted by hand is still registered in git; rather than `git
  worktree remove`, which deletes its administrative directory with the
  submodule git directories in `modules/`, that directory is moved to
  `<common>/archon-set-aside/<name>-<n>` with every `core.worktree` under
  its `modules/` removed, a detached `HEAD` named
  `refs/archon/saved/<commit>`, and the move posted to the run's progress
  (`KeptWork::Orphaned`). First, an operation it left in progress (rebase, `am`,
  cherry-pick, revert, bisect, merge) is ended with its `--quit` (`bisect
  reset HEAD`), which keeps files, index and `HEAD`. Then anything it left
  uncommitted, untracked files included, is committed as
  `archon: save uncommitted work left on <branch>`: onto what it had
  checked out when that is a worker branch of the same role
  (`team::worker_branch_role`) or a detached `HEAD`. Any other branch (the
  operator's `main`, another role's) never moves: `HEAD` is first detached
  from it with `update-ref --no-deref`, which keeps files and index. A
  detached `HEAD` is named `refs/archon/saved/<commit>` so gc keeps it.
  That work is kept but never merged. A git failure fails the slot like a
  spawn refused by Aether. The worktree is made ready only while the spawn
  is unsent (`ParentBinding::Pending`): once its request may have reached
  Aether (a lost reply, a crash), its agent may be live there, so a
  re-issue sends the stored request and leaves the worktree alone.
- **Roster.** Once every role of the attempt has an agent (the latest
  stage run of each slot), each running role is sent one roster with
  `send_message`: every role's name, agent id, worktree and branch
  (`executor::roster`). Its version is 1 plus the number of agents
  replaced in the attempt, derived from the persisted stage runs, and
  `kv.team_roster:<stage run>` records the last version a stage run
  received. A replacement's spawn and every sweep deliver what is owed,
  so a new version reaches every live role, and a role that finished
  gets none. Each send is keyed `roster:<stage run>:v<version>`, so a
  retry after a crash or a lost reply is delivered once. An unreachable
  Aether leaves it owed for the next sweep; an agent Aether no longer
  knows, or a refusal (posted to the run's progress), marks it sent.
  Records of settled stage runs are pruned.
- **Sibling cancel.** When the driver cancels a failed role's live
  siblings (§6.4), the executor persists each as `cancelled` with the
  reason, stores `kv.sibling_stop:<stage run>` (agent and reason) in the
  same store lock, and posts one progress line naming the cancelled
  roles and why. The agent's `cancel` follows outside the lock; the key
  is deleted once Aether confirms, no longer knows the agent, or refuses,
  and kept for the next sweep when Aether is unreachable, so a crash or
  an outage only delays the stop. When the decision is `Finish`, the run's
  own stop cancels the agents.
- **Committed run worktree.** Roles fork from the run branch's last commit,
  so a fork is refused while the run worktree has uncommitted changes to
  tracked files, naming them. The slot fails, and the files reach the
  `on_fail` target as `{{feedback}}`. The contract of every work stage of a
  team pipeline tells it to commit on the run branch.
- **Budget.** Each slot's Archon grant is the item's remaining budget
  divided by the number of roles, read when that slot is reserved. The grant
  becomes an Aether `cost_budget` only for a cost-reporting model (§5.3). An
  infra replacement gets the failed slot's grant less what it
  spent (capped by the remainder), and none when that is used up, since
  the siblings still hold their shares.
- **Merge.** When every slot completed, the driver emits
  `MergeTeam(MergePlan)`. The executor queues it (`RunTable.merges`), runs
  git without holding a lock, then persists the outcome in
  `kv.team_merge:<run>:<stage>:<attempt>` before the driver acts on it. The
  merge checks that the run worktree has the run branch checked out,
  aborts a merge a crash left in progress, and refuses uncommitted tracked
  changes. It settles each role's worktree: one with another branch
  checked out, unmerged files or an operation in progress is recorded as
  `Refused { role, reason }` before anything merges. Otherwise what the
  role left uncommitted is committed onto its branch as
  `archon: commit the work role <role> left uncommitted`, only when
  something is staged, and never untracked nested repositories (which
  `add --all` would commit as gitlinks with no `.gitmodules`). What git
  still reports changed then (a nested repository, or changes inside a
  submodule's tracked files; a submodule holding only untracked files,
  such as build output, is ignored) cannot reach the branch, so it is
  `Refused` too. Then it runs
  `git merge --no-ff` of each role's branch in `roles` order, skipping any branch already an ancestor of `HEAD`
  (`merge-base --is-ancestor`). The daemon commits these merges itself:
  with `core.hooksPath=/dev/null`, so no repository hook runs in the
  daemon, and as `Archon <archon@localhost>` only when git has no identity
  configured.
- **Conflict.** A conflicting merge is aborted with `git merge --abort` and
  recorded as `Conflict { role, files }`; the driver fails the stage with
  the files as `{{feedback}}` for its `on_fail` target, or fails the run.
  A `Refused` outcome does the same with its reason. Untracked files in the
  run worktree that a role's branch adds, or that are in the way of a file
  or directory it adds, are reported as a conflict before git is asked to
  merge over them. Roles merged before the conflict stay on the run
  branch. The feedback lists every role's branch, so the next attempt,
  which forks afresh, can merge or cherry-pick earlier work. Paths are read
  with `-z`, so they reach the feedback unescaped.
- **Saved work.** Every commit the daemon makes of what a role left
  uncommitted, before a merge or a new attempt, is posted to the run's
  progress in the feed (`SavedWork::describe`): the worktree, the branch,
  the commit and the files, so the operator can spot a stray `.env`. The
  commit is made by then and a retry finds nothing left to commit, so a
  notice that cannot be posted is logged as an error instead of failing
  the slot or the merge. At the merge it is posted once the outcome is
  persisted.
- **Failure.** Any other git failure fails the run as a blocker. Daemon
  commits follow `commit.gpgsign` and run without a timeout, so signing
  must not prompt. If
  persisting the outcome or acting on it fails, the merge is queued again
  and the run's driver dropped, so the sweep's retry rebuilds it from the
  store. A retry whose outcome was persisted reuses it without running git,
  since the next attempt's roles may be forking from the run worktree.

A restart between merges finds no persisted outcome, so the rebuilt driver
emits `MergeTeam` again and the merge skips what already landed. After the
outcome is persisted, the driver replays it and never merges again. Worker
worktrees are not removed when the run ends; worktree gc collects them
with the run's other worktrees (§13.2).

### 13.2 Worktree gc

One pass, `archon_daemon::gc`, collects what finished runs leave on disk.
It runs three ways:

- `archond` runs it at startup and every 30 minutes with `worktrees.auto_gc`
  on (the default), and before a dispatch the disk hold holds (§7.3).
- `archon gc [--older-than AGE] [--force]` asks the running `archond`
  (`Request::Gc`, operator-only, served in standby too) and prints what
  was removed, whose caches were deleted, the space freed, and what was
  skipped and why.
- `archon gc --offline`, or `archon gc` when no `archond` answers, collects
  without it. It takes the daemon lock (`<socket>.lock`), so it refuses
  while an `archond` runs and none starts meanwhile, and opens the store
  read-only, refusing one of another schema version. A store `archond`
  closed cleanly, with no `archon.db-wal` beside it, is opened immutable,
  so no `-wal` or `-shm` file has to be created on the full disk. `archon gc` never
  starts `archond`, which would fail on the full disk that is the usual
  reason to collect.

A pass looks at terminal items last changed before the later of its two
cutoffs, and at each workspace whose runs have all finished:

- `worktrees.cache_after` (default 1h) after its last run finished, a
  worktree's build caches go: what `git clean -dX` deletes, the files git
  ignores, but no nested repository, and with `worktrees.cache_dirs` set
  only those inside the listed directories. Untracked source, changes and
  commits stay. A worktree a team run set aside runs no git, so only the
  `cache_dirs` in it that are real directories, reached through no
  symlink, go.
- `worktrees.gc_after` (default 7d, or `--older-than`) after, the worktree
  goes with `git worktree remove`, comparing symlink-resolved paths as git
  does. One with uncommitted changes is kept and reported as skipped, with
  its caches deleted, unless `--force`. A scratch workspace under
  `<data>/scratch` is deleted then too.
- A succeeded, shipped run's worktree that `archond` should have removed after
  shipping (§13) but did not, having crashed between finishing the run and
  the cleanup, goes as soon as its caches are due. Only that worktree
  does: a team run's role worktrees and what it set aside wait for
  `gc_after` like any other run's.
  Shipping is recorded when a ship-kind stage from the frozen pipeline
  completed, regardless of its stage id. A repository's ship mode alone
  never triggers early removal. Runs recorded before shipping proof was
  added use normal retention. Successful non-shipping runs, including
  report-only scratch and clean worktree runs, wait for `gc_after`, or an
  explicit `--older-than` cutoff. Published report copies (§14) are outside
  the workspace and survive every gc path, including `--force` and offline gc.
  Pages published from their original files after a copy failure (§14) remain
  subject to workspace retention.
- A snapshot a `repo-readonly` run left behind (§13; its removal failed,
  or `archond` stopped first) goes whole as soon as its caches are due,
  dirty or locked, with or without `--force`: `git worktree remove --force
  --force`, then its item directory once empty. It goes only when it is a
  real directory, not a symlink, whose real path is still
  `<item-id>/snapshot` for its own item inside `worktrees.root`, and a
  worktree git lists for the repository.

Only paths inside `worktrees.root` that git lists as worktrees of the
item's repository, scratch directories under the data dir, and what a team
run or a fresh retry set aside are touched; the repository itself (where
`repo-readonly` stages worked before they had snapshots) never is. The
repository is the registered one. For an item whose
repository was unregistered since, it is the common git directory the
worktree's `.git` file names, so its worktrees are still removed through
git.

For a team run (§13.1) this covers everything under its `team_root`: the
run worktree `main` and every `<stage>-<role>` worktree, by the same rules,
plus each directory in `<item>/.set-aside/` and each git directory in
`<common>/archon-set-aside/` whose `gitdir` file names a worktree directly
under that root. A flat item's run worktree brings each
`<worktrees>/<repo>/.set-aside/<item-id>-<n>` a fresh retry left and each
git directory in `<common>/archon-set-aside/` whose `gitdir` file names
that worktree. Set-aside directories are deleted whole, and only when
they resolve strictly inside a real (not symlinked) `.set-aside` or
`archon-set-aside` directory. A `main` a shipped run already removed is not
reported again, and an `<item>` directory gc leaves empty (but for an
empty `.set-aside`) is removed too, so no stray directory meets a later run
of the item. A team root that is itself a symlink is never followed:
nothing under it is collected, it is reported as skipped, and a removed
root is reported by its real path.

A live run's directories are never touched. Items that are not terminal,
or changed after the cutoff, are not looked at. Holding the run table,
which an item retry and a dispatch take too, `archond` checks that the
store still shows the item terminal with the `updated_at` and the number
of runs the pass planned with, and marks it as being collected
(`DaemonContext::collecting`); an item retried in between, whose new run
would reuse the worktree, is left alone, even when a clock stepped back
leaves its `updated_at` as it was. It then collects the item without the
run table, so git and the filesystem hold up no live run. A retry of a
marked item still queues it, but the dispatcher leaves it queued until the
mark is gone, which pokes dispatch; a fresh retry of it, which would move
the worktree the pass is removing, is refused until then. A pass stopped
mid-sweep (the workers stopped for standby, or the held-dispatch pass out
of time) leaves that sweep to finish with its mark, which pokes dispatch
as it goes all the same; a later pass leaves a marked item to it and reports
its places as skipped. Offline, the daemon lock rules out any run. Branches,
role branches and `refs/archon/saved/*` are kept.

Only a failed removal or cache deletion, or a place gc cannot read (a
registered repository that is no longer a git repository, an unreadable
set-aside directory), makes `archon gc` exit non-zero. Each is reported
against its path while gc still collects everything else; `archond` logs
them for its own passes, which a store error stops like any worker (§16).

### 13.3 The orchestrator's mirrors

The orchestrator answers a question about code by reading the code, so it
needs every registered repository at hand, and it must not hold the
operator's checkout. `archon_daemon::mirror` keeps
`<data>/orchestrator/repos/<name>` for each registered repository: a
`git clone --local --no-checkout` of the repository's path, checked out
detached at the base branch (`repo.base_branch`, else the repository's
`HEAD` commit, as for a worktree). Like a worktree it holds committed work
only.

**What a mirror cannot do.**

- It has no remote. `origin` is removed after the clone and the daemon
  fetches by path, so `git push`, `git pull` and `git fetch` have nowhere
  to go, and the repository's own remotes and credentials are not in its
  config.
- It has no branch of the repository's. The clone's `refs/heads/*` are
  deleted; the repository's branches are fetched to `refs/mirror/*`, a
  namespace no push or checkout names by default, and a base that is not a
  branch to `refs/archon/base`.
- A push is refused anyway. Every mirror's `core.hooksPath` is
  `<data>/mirror-hooks`, outside the orchestrator's workspace, whose
  `pre-push` hook exits 1, so a helper that adds a remote still cannot push
  through it. A worktree of the mirror shares that config. `--no-verify`
  skips a hook, so this guards against a mistake and is no boundary: an
  agent runs as the operator's user, and what its tools can reach beyond
  its workspace is Aether's limit (§17). What a mirror guarantees is that
  nothing in it points at the repository or its remotes.
- No hook of the repository runs. `--template=` copies none, a
  repository's `.git/hooks` is never cloned, and the daemon's own git in a
  mirror runs with `core.hooksPath=/dev/null`, without the operator's
  global and system config, and with `--git-dir` and `--work-tree` naming
  the mirror itself, so a `core.worktree` written into its config cannot
  aim a checkout elsewhere.
- A symbolic link is never followed. A link where the mirrors directory or
  a mirror belongs, or a `.git` that is a file or a link, is removed and
  the mirror cloned again. A link inside a mirror is a file of the
  repository like any other.

Objects are shared with the repository by hard links where the filesystem
allows, so a large repository costs its checkout and no second object
store, and a mirror on another filesystem costs a copy. A linked object is
one read-only file under two names: git never rewrites one, and nothing git
does in a mirror writes to the repository. Only a process that set out to
overwrite an object file in place would damage both.

**Refresh.** One worker (`mirror::run`) brings every mirror up to date at
start and when the operator sends a message. It refreshes one repository's
mirror when that repository is registered or changed and when one of its
items finishes (its pull request may have moved the base). Every pass also
retries a mirror that has a problem or was never made. Requests made
during a pass share the next one, and a request never waits for the
refresh. A pass resolves the base in the repository, skips
the fetch when the mirror is already there, fetches only what it lacks
otherwise, and checks out with `--force --detach`: tracked files a helper
changed are reset, untracked ones are left. A clone is made beside its
place and renamed, so a crash leaves no half-made mirror, and the pass
removes whatever else lies in the mirrors directory, an unregistered
repository's mirror included.

**A refresh that fails** leaves the mirror where it was and is never
silent: one `System` feed entry names the repository and git's error, and
a quiet `mirror` event with `state: "stale"` joins the orchestrator's next
trigger (§5.4), so it knows the code it reads may be behind. The state is
kept in kv `mirror_<repo id>`, so the problem is told once, and once more
(`current`) when a later refresh succeeds. A repository whose `[models]`
refuses the orchestrator's own model gets no mirror at all (`withheld`):
the orchestrator may not read it, and a question about it is an `ask` item
under that repository's policy.

**Helpers.** A helper's `workspace_root` may be a mirror. Aether accepts it,
since it is under the orchestrator's workspace, and an isolated child gets
a git worktree of the mirror on a new branch of the mirror
(`aether-runtime`'s `workspace.rs`), which shares the mirror's config:
still no remote, still the push guard. The watchdog allows a helper there,
holds it to that repository's model policy, and cancels one placed in the
registered repository itself or under the worktrees root (§7.4).

## 14. Lavish plugin

`archon-lavish` wraps the `lavish-axi` CLI. It is optional and degrades
cleanly.

```rust
pub struct Lavish { binary: PathBuf }
impl Lavish {
    pub fn detect(cfg: &LavishConfig) -> Result<Option<Lavish>>;              // None when off or not found
    pub async fn open(&self, html: &Path) -> Result<LavishSession>;           // `lavish-axi <html>` → session URL
    pub async fn poll(&self, html: &Path, reply: Option<&str>) -> Result<LavishFeedback>; // blocks until feedback
    pub async fn end(&self, html: &Path) -> Result<()>;
}
pub enum LavishFeedback { Prompts { raw: String, prompts: Vec<String> }, Ended { raw: String, prompts: Vec<String> },
                          Disconnected { raw: String }, ArtifactFailure { raw: String } }
```

- `publish_report{title, path, item?}`: the report must be an HTML file whose
  assets sit in the same directory. The orchestrator or a report stage writes
  it under `<workspace>/.archon/reports/<name>/index.html`. With Lavish
  available, `archond` opens the durable copy, records a
  `Report{id, path, url, item}`, adds
  a `Decision` feed entry with the URL, and starts **one supervised poll task
  per open report**. Each poll is a child process owned by `archond` and is
  re-run on exit until the session ends.
- Before opening or announcing a report, `archond` copies its whole directory
  (HTML and relative assets, including nested screenshots) to
  `<data>/reports/<report-id>/`. Lavish and the plain-path fallback use this
  copy, so deleting the run workspace cannot delete the published page.
  Republishing the original file keeps its id and atomically replaces the
  directory, refreshing assets and removing assets no longer present. A
  report that passes HTML validation but cannot be copied is still published
  from its original file. A System note names the report and the copy failure;
  gate page text repeats it. Such a page is **not durable**: it disappears when
  its workspace is collected. Republishing switches it to a durable copy once
  copying succeeds. Copying refuses source paths containing `..` or symlinks,
  symlinks inside the directory, special files, and directories containing the archive.
  Each copy is limited to 64 MiB, 4096 entries and 32 nested directory levels;
  files outside that directory are never copied. Invalid HTML paths and stage
  pages outside the workspace remain visible refusals. For gated stages, System
  notes and the gate question name every refused listing and its reason, even
  when another page is shown. A publication failure
  before recording removes a first publication's snapshot or restores the
  previous copy on refresh. A legacy or fallback report with no prior copy
  keeps its record and leaves the unused new copy for orphan pruning. Once
  recorded, its snapshot is retained even if announcing fails.
  Older records without a copy continue using their original path while it
  exists; republishing them creates a copy under the same id.
- One HTML file is one report. Publishing a file again, from a gate, a
  stage's `reports[]` or `publish_report`, keeps its report id, and
  for an already copied report, `lavish-axi <html>` resumes the file's
  session rather than starting a second one. Every open reopen question about the file is closed then
  (`withdrawn: the report was shown again`), because the page is showing
  again. A reopen question names its page's path, so one an older version
  left behind, which no record refers to any more, is found and closed too.
- Feedback goes to the orchestrator as a `lavish_feedback` trigger carrying
  the operator's prompts, parsed best effort from the whole poll output, and
  that output without its top-level `dom_snapshot` field, which is replaced
  by a note of its size: the page is the report file. The session,
  prompts, artifact failures and next step all stay. As a backstop the text
  is bounded at 4 KB, cut on a character boundary with a note of what was
  left out. lavish-axi writes its `next_step` instructions after the
  prompts, so a cut takes those first; each prompt's target and
  `attachments` paths, which `prompts` lacks, are cut only if the prompts
  alone pass the bound. `archond` logs the whole output at debug level
  (`RUST_LOG=archon_daemon=debug`) before trimming it. Prompts on a page
  that an open operator gate shows decide that gate instead, as a chat
  answer would (§18.3); a prompt left empty answers nothing and is dropped.
  `Ended` closes the report.
- `Disconnected` (the browser window gone past Lavish's grace period) marks
  the report `disconnected`. Its session stays resumable. Only a report
  that an open operator gate waits on (a proposal, or a `before-pr` change
  report) asks a `Question` whether to reopen it, and at most one per
  report. `archond` itself acts on the answer (`reopen` opens and polls it
  again) rather than triggering the orchestrator. If Lavish cannot show it,
  a `System` note says so and the report stays `disconnected`.
  Informational pages, such as a `with-pr` change report or a page the
  orchestrator published, ask nothing. `archon reports` lists every report
  with its state and URL. `archon reports reopen <rpt_…>` (`ReopenReport`)
  shows one again with `lavish-axi <html> --reopen`, because the operator
  asked, so a session they ended from the browser reopens too. If the file
  is gone or Lavish fails, the request fails and the record is left
  unchanged. Without Lavish it returns the file path and changes nothing.
- A reopen question is closed with `withdrawn: no gate waits on the report
  any more` as soon as no open gate waits on its report: when the gate is
  decided, when its run finishes (right after the finish withdraws the
  gate, §18.3), and at startup, which also closes questions older versions
  asked about informational pages. The same sweep closes any reopen
  question no report asks any more (older versions orphaned one when a page
  was published again): `withdrawn: the report no longer asks it` while its
  page's gate waits on a newer question, otherwise the no-gate text. An
  answer to such an orphan is still Archon's: it closes the question,
  reopens nothing (a `System` note names `archon reports reopen` when the
  answer asked to) and never reaches the orchestrator.
- A poll's failure is its report's alone; only a store failure reaches the
  event pump. A failing `lavish-axi poll`, or feedback Archon cannot act on
  (a `System` note says why), is retried with backoff from 1 s to 30 s. A
  poll task that panics is restarted the same way. After 5 failures in a
  row the report is `stopped` and a `System` note gives its path.
- When a report's item finishes, `archond` stops its poll, runs
  `lavish-axi end <html>` and then records the report `ended`, so no poll
  process outlives its item. The report file is kept. The same check runs
  at startup for items that finished while `archond` was down, so a
  session whose `end` a stop interrupted is ended then. A report shown
  again meanwhile keeps its new state. A report that
  an open gate still waits on stays open, and so does one shown only after
  its item had finished, because it was meant for review then. An agent's
  `end` still allows a plain reopen. If Lavish cannot end the session, a
  `System` note says so and the report stays `ended`.
- Each report is one `kv.report:<id>` record, so a change rewrites only
  that record. The single `kv.reports` map of earlier versions is split on
  first use. A settled report (ended, stopped, shown only as a path, or
  disconnected with no reopen question pending) is pruned 7 days after its
  last change (§16); until then a republished file keeps its id. An ended
  or disconnected report can still be reopened, so it is kept past the 7
  days for as long as its HTML file exists. `archon reports` states this
  rule. Pruning removes the durable directory together with its record,
  never the original source. Ended and disconnected copies stay resumable
  independently of workspace retention. Under the same `report_files` lock
  used for publication, pruning also removes unreferenced `<report-id>` and
  `.<report-id>-<report-id>` staging directories left by cancellation, crashes
  or incomplete cleanup. Only directories whose mtime precedes the prune
  cutoff are removed; other names and symlinks are left alone. Fallback
  records never cause their original files to be removed.
- Without Lavish, `publish_report` returns `{published:false, path}` and a
  feed `Decision` shows the file path. Nothing fails.
- Pipelines never call Lavish directly. Report stages produce files, and the
  daemon publishes the files a stage lists in `structured.reports[]`.

## 15. Benchmarks

`bench/` is the `archon-bench` crate plus task data.

```
bench/
  Cargo.toml                   # archon-bench (bin)
  src/                         # runner, arms, graders, report
  tasks/<task-id>/
    task.toml                  # id, title, prompt, timeout_minutes, pipeline, tags
    fixture/                   # initial repository contents (git init'd per trial)
    hidden/                    # hidden tests, copied into the grading copy only
    reference.patch|.answer    # the reference solution (`archon-bench verify`)
    rubric.json                # answer tasks: proximity keyword groups
    grade.sh                   # run on the grading copy; exit 0 = pass; optional JSON on stdout
  graders/                     # expect_tests.py (runner summaries), rubric.py
  results/                     # gitignored: <stamp>/results.jsonl, summary.md, report.html, trials/*/logs
  .work/                       # gitignored: scratch for the crate's own tests only
```

Arms. The model is fixed per run with `--model`, and effort with `--effort`:

| Arm | How a trial runs | Tokens/cost from | Operator-visible messages |
| --- | --- | --- | --- |
| `archon` | Fresh `ARCHON_DATA_DIR`, repo registered, operator message sent via `Say`, wait for all items terminal. | Spend ledger (all stage agents + orchestrator). | Count of default-visible feed items excluding the operator's own. |
| `aether` | `create_agent` single root agent with the task prompt in the fixture workspace, wait for its task. | `agent.usage_updated` totals. | Assistant messages of the root agent. |
| `claude` | `claude -p <prompt> --output-format json --model <m> --dangerously-skip-permissions` in the workspace. | JSON `usage` and `total_cost_usd`. | 1 (the final result). |
| `codex` | `codex exec --json --dangerously-bypass-approvals-and-sandbox -m <m> <prompt>` in the workspace. | The last `turn.completed` usage (a running total). | 1. |

Isolation. Agents must never reach hidden tests, reference solutions,
rubrics, other trials or earlier results. Trial workspaces therefore live
outside the checkout, under `--work-dir` (default
`$XDG_DATA_HOME/archon-bench/work`). A work dir inside the checkout, the task
suite, the results directory or /tmp is refused. Each run gets a
traverse-only (0711) directory `<work>/<stamp>/`. Each trial gets an
unguessably named workspace in it, holding only the fixtures: each copied,
`git init`ed, committed once and tagged `bench-base`. The archon arm keeps
its private daemon state in the workspace's `.archon/`.

Agent processes run in their own process group, which is killed when the
agent exits. They get `GIT_CEILING_DIRECTORIES` set to the workspace. They
lose every variable and `PATH` entry pointing into a protected directory,
along with `CARGO_*` except `CARGO_HOME` and `CARGO_BUILD_JOBS`. There is no
OS sandbox: an agent that searched the whole disk could still find the
checkout.

Grading. After the arm stopped, the result is copied into a second
unguessable directory and `grade.sh` runs there with `$BENCH_TASK_DIR` and
`$BENCH_GRADERS`; the copy is deleted afterwards. For `aether`, `claude` and
`codex`, the result is the workspace, including uncommitted changes. For
`archon`, it is each item's run branch merged onto `bench-base` (for
multi-repo tasks, one per fixture, passed as `$BENCH_RESULT_<FIXTURE>`).

Graders restore visible tests from the task directory, never from the
agent's repository. `graders/expect_tests.py` requires the runner's own
summary to report no failures and a minimum number of passing tests, so
early exits, skipped tests or tests switched off fail. Rubric groups need
related facts within a window of each other, and over-long answers fail.
Logs, `answer.txt` and each repository's `<repo>.result.patch` (the diff
from `bench-base`) stay under `results/<stamp>/trials/<task>/<arm>/<n>/logs/`.
Workspaces are deleted after grading unless `--keep-workspaces` is passed.

Metrics per trial: `success` (grader exit status), `score` (optional grader
JSON), `wall_secs`, `input_tokens`, `output_tokens`, `cost_usd`,
`estimated_cost_usd`, `operator_messages`, `error`. Results go to
`results.jsonl`, one JSON object per trial. A failed trial with an `error`
(a provider limit, a crash, a grading failure) is an infrastructure failure:
`summary.md` leaves it out of every aggregate and lists it on its own. Over
the graded trials, `summary.md` aggregates mean ± sample stdev per arm and
task, marks metrics only some trials reported with `(n=…)`, and reports
unbiased pass@k per task averaged over tasks. Archon's
wall time excludes daemon start-up and the quiet window that detects the
end. With `--lavish`, the HTML report is published.
Trials run sequentially by default. `--parallel N` is capped by the machine.

The v1 suite has at least 6 tasks: bug fix with failing test, feature with
spec tests, multi-file refactor keeping tests green, multi-repo change (two
fixtures, API and client), a question-only task (`ask` arm grading by keyword
rubric), and a review task (seeded bug must be reported).

## 16. Failure modes and recovery

| Failure | Detection | Recovery |
| --- | --- | --- |
| `archond` crash or restart | Startup. | Open the store, then reconcile. `dispatching` items without a run go back to `queued`. For `spawning` stage runs, re-issue `create_agent` with the stored request under the same key (Aether dedupes): a `bound` one exactly as stored, a `pending` one after binding it to the run's node, which `kv.run_node:<run>` names and `get_agent` confirms live (§4.5). A node create whose reply was lost is replayed from its stored request. For `running` stage runs, `get_task`, and feed any settled outcome to the driver. Rebuild drivers with `RunDriver::resume`. Resubscribe from cursors. Finished runs whose node was not retired are retired by the first sweep, once none of their stage agents is still live in Aether (§4.5). A shipped run's worktree whose removal the crash lost is removed by worktree gc (§13.2). |
| Run node lost (Aether wiped, or cancelled in its TUI) | `get_agent` before the run's next stage is bound. | Create a replacement under `<run>:node:<generation+1>` and parent the stage agent under it (§4.5). Aether's cancel cascade has already ended the node's live stage agents; their `agent.cancelled` events settle their stages as cancelled, which the driver treats as failed work. A stage already bound to the lost node is refused when re-issued and fails. |
| Run node cancel unanswered or failed | `cancel` returns `unavailable` or an error at retirement. | The `kv.run_node:<run>` entry stays; every dispatch sweep retires finished runs with no live stage until the cancel is answered (§4.5). |
| Stage agent still ending its turn when its run finishes | `get_agent` at retirement reports it live. | The node is left for the next sweep, up to 60 s after that stage settled; then it is cancelled regardless (§4.5). Only agents settled within the last 60 s are asked, newest first. |
| Aether's `[network] default` changes mid-run | `status.network_default`, read each time a stage is bound to its node. | A stage bound afterwards states the narrower of its node's network and the new default: a tightened default is honoured at once, and a loosened one never widens the node, which Aether would refuse. A stage bound before the change is replayed exactly as stored (§4.5). |
| Archon's own `aetherd` cannot start (dedicated mode) | `aether daemon` exits non-zero, or nothing listens within 10 s. | The command fails with Aether's own stderr. `archond` is not started by that command. `archon doctor` shows the instance's state. |
| Aether unreachable at start | Connect error. | Serve the Archon socket anyway. `Status` shows `aether: disconnected`. Reconnect with backoff from 1s to 30s; while nothing listens on a local socket (it is missing, or refuses) every second instead, logged once, so an aetherd that starts is found at once. No dispatch while disconnected. An `archon` command that started or restarted Archon's own aetherd, or waited while another did, waits up to 5 s for `archond` to report the link before it runs, and says so when that runs out. |
| Aether connection drops | Reader EOF or id-0 notice. | Same backoff. On reconnect, a changed `instance_id` means Aether restarted: run reconciliation (`get_agent` and `get_task` for every live stage agent and the orchestrator). |
| Aether restart kills native processes | `agent.failed` or an interrupted task. | Stage: infra retry (§6.3). Orchestrator: the supervisor path below. |
| Orchestrator failed or cancelled | Orchestrator subscription, or `get_agent` at start. | `wake` in place. If refused, `restart` (new agent from checkpoint), store the new id, and write a `System` feed entry. The first revival of a streak runs at once; each later one waits 10s, 30s, 2m, 5m, then every 10m (`supervisor::REVIVAL_BACKOFF`). A turn that ends `ready` or `waiting` closes the streak. After 3 revivals in a row, post one `Blocker`; revival never stops for good, and `archon orchestrator restart` starts a fresh agent at once. Dispatch continues, because the queue does not depend on the orchestrator. |
| Orchestrator restarted in Aether (`aether restart`) | `agent.restarted` on the orchestrator's subscription, or `restarted_as` at start and on reconnect. | Adopt the successor if its role is `archon-orchestrator`: store its id, close the failure streak, move the subscription to it, and write a `System` feed entry. Its held news follows (§5.4). A successor with another role is not adopted. A chain of restarts is followed to its newest orchestrator, and every agent left behind on the way is retired (next row). |
| `archon` MCP server died in Aether (after a redeploy, say) | `mcp_servers` reports it `failed`: at every connect to Aether and every 60 s (`supervisor::MCP_CHECK_INTERVAL`). | `refresh` it, then restart the orchestrator so it gets the tools back. Both back off like revivals, and 3 failures in a row post one `Blocker` (below). |
| Orchestrator replaced while its delegated children run | Any path above that puts a new agent in charge. | The children end. Aether's `cancel` takes an agent's live descendants along; a failure does not, and neither does Aether's `restart`. So every agent that stops being the orchestrator is written to `kv.orchestrator_retired`, with the sweep it is owed, before its successor is stored, and Archon sends `cancel` for it, which Aether applies to the subtree of an agent already stopped. An adoption retires every agent between the stored orchestrator and the adopted one, each with children of its own; a manual restart and a relink follow the same chain first, so they act on the agent in charge rather than on one Aether already replaced. A background task makes the owed sweeps from `archond`'s start on: at once when one becomes owed or Aether reconnects, then with the revival backoff while it fails. A sweep is done only when Aether answers its `cancel`, no longer knows the agent, or reports it ended. Its first failure and its later success are each a `System` entry. A sweep holds no lifecycle lock, so it never delays the orchestrator in charge, which is never swept. A record that cannot be read is reset with a `Blocker` that quotes it, rather than stopping the orchestrator. Records stay through the 7-day event retention, so a retired agent's late events and its children's spend are still Archon's, and are then pruned unless a sweep is still owed. The new agent does not know what was delegated. A `wake` in place keeps the children. |
| archond's delivery of a run fails | `git push` or `gh` exits non-zero or times out, or a check of the run worktree does not hold (§8.7). | A check that does not hold, a remote branch with other commits, or several open pull requests fail the item as `stage ship: <what to change>`; missing access, a missing `origin` or a branch rule as `stage ship: BLOCKED: …`, which is a `Blocker`. Anything else is tried again by the sweep after 15 s, doubling, five times in all, then fails the item with the last error. A retry of the item delivers the same branch: nothing is forced, and the open pull request is reused. |
| `archond` stops while it delivers a run | Startup: the run is `running` at its last stage and every stage passed. | The resumed driver asks for the delivery again and the sweep makes it. With `kv.ship:<run>` written, the outcome is replayed and nothing is delivered; without it the checks, the push and the pull request lookup run again, which push nothing `origin` has and find the pull request GitHub lists, so no second one is opened. |
| A delegating stage agent settles while its children run | The stage's settle: its stored request has `leaf: false`. | `kv.descendant_stop:<stage run>` is written with the settle. No agent of that run is created (next stage or replacement), and archond does not deliver the run (§8.7), until Archon has cancelled the settled agent and Aether holds no agent beneath it in a non-terminal state; the agent is first left to end its own turn, up to 60 s. The cancel is sent once; the check is repeated each dispatcher pass and after a restart. Agents still running 120 s after the cancel are named in one `Blocker` and the run keeps waiting; a refused cancel fails the run with a `Blocker` (§6.3). |
| Aether names no caller on Archon's tools | An unnamed call that changes Archon reaches `archond`. | One `Blocker` with the fix; such calls are accepted only from an orchestrator that cannot delegate, and new generations start without `delegate`, until the orchestrator makes a named call of any kind (§11). In dedicated mode the registration is repaired and Archon's `aetherd` restarted by the next command (§4.7). |
| Orchestrator missing (first start) | No `kv.orchestrator_agent`. | Create it (§4.4). |
| `archon mcp` cannot reach `archond` | Connect error in the proxy. | `connect_or_start` autostarts `archond` (`archon daemon run --detach`, which waits until the daemon listens and fails as soon as it exits). If that fails, return a tool error with the launcher's stderr and the startup log. |
| Stage exceeds deadline | Timer. | `cancel` the agent, mark the stage `timed_out`, and let the driver treat it as a failed verdict or failed work. |
| A steer is stored but Aether does not take what the live agents are owed | `update_agent`, `interrupt` or `send_message` unanswered, or a crash after the store. | The steps stay in `kv.steer_owed:<stage run>` and every sweep repeats them in order, each message under its own key so it is delivered once. A step Aether refuses is dropped with a progress note; what the steer stored reaches the stage's next agents (§6.6). |
| `archond` restarts on a steered run | Startup reconcile. | `frozen::thaw` applies the run's `run_edits` to its frozen pipeline and the driver takes its recorded aborts before `RunDriver::resume`, so the run has the stages and decisions the steers left (§6.6). |
| A kept agent cannot take its fix attempt | `get_agent` reports it not live, or Aether refuses the deadline or the message. | The agent is cancelled and a fresh one takes the attempt with the findings and the digest of the last attempt (§6.7). An unanswered call is repeated under the same key instead. |
| A resumed attempt's agent dies or ends its turn saying nothing | `resumed_outcome` at the agent's turn end and at every sweep. | Infra retry (§6.3): the kept agent is stopped and a fresh agent takes the slot. |
| A resumed attempt's final message leaves out the ship record it owes | `ShipRecord::read` of its `structured` as it settles. | The agent is asked once for the record alone (§6.7); a second miss fails the stage, saying what is wrong with the record. |
| Worktree creation fails | git exit status. | Item `failed` with git's stderr. The feed gets a `Blocker`. |
| Disk low or full | Free space below `limits.min_free_gb` at a dispatch pass, or a stage or `create_agent` failed with `database or disk is full`, `No space left on device` or `ENOSPC`. | The disk hold (§7.3): no dispatch, and one `Blocker` with the free space, the path and `archon gc`. With `worktrees.auto_gc` on, a worktree gc pass (§13.2) runs before the held dispatch. When `archond` cannot start, `archon gc --offline` collects without it. A stage that failed on a full disk is retried once space is back, without using an infra retry (§6.3); a `create_agent` that keeps failing so fails its run after 5 tries. |
| Archon's store on a full disk | SQLite `SQLITE_FULL`, mapped to `ArchonError::DiskFull` (`store: disk full: ...`). | Standby: the pump, dispatcher, trigger delivery, pruning and `archond`'s own gc passes stop, since memory may no longer match a store whose write failed. The socket keeps serving reads (`status`, items, repos, pipelines, questions, the feed, reports, the spend report), so `archon status` and `archon gc` work, and refuses everything else with the `DiskFull` error. Staying up also keeps clients that autostart `archond` from restarting it in a loop. Once free space ends the disk hold, `archond` exits cleanly, and the next command starts it, which recovers as after a crash. A store that fails to open on a full disk ends the start with the same error. |
| Budget exhausted | Ledger update. | Daily/total budgets stop dispatching while running agents finish. An exhausted item budget fails its run and cancels its live agents. Post a `Blocker` and a trigger. Aether's own `cost_budget` also stops the agents that carried one. |
| Model policy violated | §7.4. | Refuse the request, or cancel the agent. `Blocker`. |
| Pipeline file broken | Catalog load. | Excluded and reported. Queued items naming it fail at dispatch with the parse error. |
| Lavish missing or crashes | `detect` or poll exit. | Degrade (§14). A poll that exits abnormally is restarted with backoff, at most 5 times, and then a `System` note is posted. |
| A report poll fails or panics | Poll error, feedback Archon cannot act on, or the poll task ends abnormally. | Contained to that report (§14): a `System` note, a retry with backoff, and after 5 failures in a row the report is `stopped`. Only a store failure stops the event pump. |
| Restart during a team merge | Startup: a passed team stage without `kv.team_merge:<run>:<stage>:<attempt>`. | The rebuilt driver emits `MergeTeam` again. The merge aborts a half-done `git merge` and skips branches already on the run branch, so nothing lands twice (§13.1). |
| Restart while a run waits at an operator gate | Startup. | The gate row and any decision are persisted, and the rebuilt driver reads them. An open gate is not asked again; one whose answer was recorded without its decision is asked again (§18.3). |
| SQLite busy | rusqlite error. | WAL mode, `busy_timeout = 5s`. Only `archond` writes, and CLI reads also go through `archond`. |
| Store growth | The pruning pass, at startup and every 10 minutes. | `kv` entries nothing reads any more are deleted (below). Only a store failure stops the pass; anything else is logged and retried next pass. |

**The `archon` MCP server.** Aether runs `archon mcp` as the `archon`
server, and it does not reconnect one whose process died: after an
`archond` redeploy the orchestrator's first tool call can fail with a broken
pipe, and every later one with an unknown tool. A running agent also keeps
the tool list it started with, so restarting the orchestrator alone does not
help, and neither does a refresh alone. The supervisor
(`supervisor::watch_mcp`) therefore asks Aether for the server's health each
time the pump connects and every 60 s:

- `ready` or `unknown` (not contacted yet): nothing to do, unless a check
  saw the server fail since the orchestrator last started; then a `ready`
  server restarts the orchestrator, whoever refreshed it.
- `failed`: send `mcp_servers {refresh:true}`. When the server comes back
  `ready`, cancel the orchestrator and `restart` it from its checkpoint (a
  fresh generation if Aether refuses), as for a revival. Nothing is
  restarted after a failed refresh, or while the server is not `ready`.
- A new orchestrator agent (a new generation, an adopted successor, a
  revival that replaced it rather than woke it in place, `archon
  orchestrator restart`) gets the server's current tools, so it ends a
  streak of failed restarts, and it clears an owed restart unless a check
  saw the server `failed` and none has seen it `ready` since. An agent
  started on a down server lacks the tools too, so it is restarted once a
  check sees the server `ready`, even when the operator refreshed it by
  hand. A successful refresh always owes a restart, since the agent running
  then may have started while the server was down. If the orchestrator is
  gone by the time a restart is due, nothing is restarted, and after a
  failure `Blocker` a `System` note says the next one starts with the tools.
- Refreshes and these restarts are each paced like revivals
  (`supervisor::REVIVAL_BACKOFF`): the first of a streak runs at once, and
  later ones wait 10 s, 30 s, 2 m, 5 m, then every 10 m, checked at the next
  60 s tick after that. A refresh streak closes once the server has been
  `ready` for 10 minutes since its last refresh; a restart streak closes
  when a restart works. After 3 failures in a row of either kind
  (`supervisor::HEALS_BEFORE_BLOCKER`), one `Blocker` says what failed and
  how to heal it by hand: `archon aether mcp refresh archon`, then
  `archon orchestrator restart`. A `System` note says when the server is
  ready again, or the orchestrator has the tools again.
- Each refresh and restart, and its outcome, writes a `System` feed entry
  and a log line, so `archon log` shows them. `archon status` shows `mcp:
  ok (22 tools)` or `mcp: FAIL <reason>`, the failed refreshes and restarts
  in a row, and the last heal (`mcp` in `--json`, with `failed_refreshes`
  and `failed_relinks`). `archon doctor` keeps its own `mcp health` check.

This state lives in memory: after an `archond` restart, the first check reads
the server's state afresh. The `archon mcp` process itself does not need
`archond` to be up when Aether starts it: it opens the socket on its first
tool call (autostarting `archond` if needed) and reopens it after a drop,
so an `archond` restart costs at most the calls in flight.

**Store pruning.** `archond` runs a pruning pass at startup and every 10
minutes, so `kv` follows live work plus a 7-day retention rather than the
store's history. Each kind is found by a primary-key range over its prefix,
so a pass costs what is still stored.

| Entry | Deleted when |
| --- | --- |
| `spawn_request:<stage run>` | Its stage run is terminal (or was never recorded); a kept agent's (§6.7) once its run's node retired as well. |
| `run_vars:<run>` | Its run is terminal (or was never inserted); only a live run's driver is rebuilt from it. |
| `team_merge:<run>:<stage>:<attempt>` | Its run is terminal (or was never inserted); like `run_vars`, only a live run's driver replays it. |
| `ship:<run>` | Its run is terminal (or was never inserted); only a live run's driver replays the delivery outcome (§8.7). |
| `run_node_agent:<agent>` | Its run finished more than 7 days ago and its node was retired (`run_node:<run>` is gone). A later event of that node at most waits the 5 s spawn grace (§4.6). |
| `link_child_<agent>`, `link_unchecked_<agent>`, `link_cancelled_<agent>` | The child's item is terminal and last changed more than 7 days ago. For the orchestrator's children, which have no item, the pass asks `get_agent` (at most 32 a pass). The first pass that finds the child ended records when, in `link_ended_<agent>`, and all its entries go 7 days after that. |
| `link_stage_agent_<agent>` | Its item is terminal and last changed more than 7 days ago (or is gone). A recorded stage agent is also found through its stage run (§4.6). |
| `report:<id>` | The report is settled and last changed more than 7 days ago (§14). An ended or disconnected report is kept while its HTML file exists. |
| An entry of `orchestrator_retired` | The orchestrator was replaced more than 7 days ago and the sweep of what it left running is no longer owed (§16). |
| `descendant_stop:<stage run>` | Not by this pass: the dispatcher deletes it once Aether has confirmed the stop (§6.3). |
| `steer_owed:<stage run>` | Not by this pass: the sweep deletes each step once Aether took it, and the rest when the stage run settled (§6.6). |
| `orchestrator_gave_up`, `orchestrator_failures` | At once: the give-up rule that wrote them was replaced by the revival backoff, and nothing reads them. |

The retention keeps what late events still need: a watched child's entry
attributes its spend and questions and lets its own children be checked,
and a pending `link_unchecked_` marker makes a replayed `agent.created`
check the child again. Entries that live state needs are never pruned:
`run_node:<run>` (deleted when the node is retired, §4.5), the held news in
`trigger_outbox` and its key counter `trigger_seq` (§5.4), the revival
streak in `orchestrator_backoff`, a replaced orchestrator whose sweep is
still owed in `orchestrator_retired`, an owed `descendant_stop:`, and the
other single keys (cursors, the orchestrator's id and
generation, the hard pause, the budget and disk holds), which do not grow
with work. An item's steer history and steered stage list
(`steers:<item>`, `shape:<item>`, §6.6) are the item's own data and stay
with it: at most 100 records and one stage list per steered item.
Questions are rows, not `kv`, so an answer not yet
delivered is never pruned, nor is a report whose reopen question waits.

## 17. Aether limitations and how Archon handles them

1. **MCP servers are daemon-global.** There is no per-agent MCP server.
   Aether can supply caller identity when a server opts into
   `send_caller_meta`; Archon requires it for stage messaging (§5.5).
   `ToolExposure::Except` hides orchestrator mutations from stage agents;
   the other exposed calls are still treated as the orchestrator's, and
   `archond` lets a mutating one through only for the current orchestrator,
   by the caller Aether names (§11).
   In the default dedicated mode, the only daemon that loads `archon mcp` is
   Archon's own (§4.7), so the operator's plain Aether agents never see
   `mcp__archon__*`. Only in shared mode can any other Aether agent that the
   operator runs see them; it can read, and change Archon only through a
   registration that sends no caller identity, while the orchestrator in
   charge cannot delegate (§11).
2. **MCP servers load at Aether start.** Aether reads `[[mcp.servers]]` only
   when `aetherd` starts, and no command reloads them. In dedicated mode,
   `archon setup` writes the instance's config before its daemon first
   starts, so no restart is needed. When Archon changes the instance's
   registration later (a repair after an upgrade, or `archon setup
   --resync` replacing the entry), it restarts the instance's `aetherd`
   itself (§4.7). In shared mode, the
   operator must restart their own `aetherd` after the first `archon setup`,
   and `archon doctor` reports "aetherd has not loaded the archon server"
   until then. `archond` calls `mcp_servers{refresh:true}` to reconnect a
   crashed `archon mcp`, but this cannot add a server Aether never loaded.
   `aetherd` spawns `archon mcp`, which inherits `aetherd`'s environment.
   That is why the dedicated instance is always started with
   `ARCHON_SOCKET` and the Archon directories set, and why in shared mode
   `ARCHON_SOCKET` must be set wherever `aetherd` starts.
3. **No isolated-worktree option on `create_agent`.** Archon manages its own
   worktrees. They start from the committed base and do not snapshot the
   operator's working tree.
4. **Native tools are not confined to `workspace_root`.** A worktree prevents
   accidental collisions. It is not a sandbox, and read-only stages are
   read-only by instruction only. They work in a disposable snapshot
   (§13), so a build or test they run writes there rather than into the
   operator's checkout, and cannot see or disturb its uncommitted work,
   but nothing stops an agent from writing outside its workspace.
   An agent the orchestrator delegates to is given a mirror (§13.3) or the
   scratch directory, never a repository: one whose scope leads into a
   registered repository or the worktrees root is cancelled (§4.4). That
   check reads the scope Aether holds for the agent; it cannot see a path a
   native tool opens outside that scope, so a helper stays out of
   repositories by its placement and its instructions, not by a sandbox.
   The same holds for a stage agent's delegated children: an editing child
   gets an isolated worktree of its own, a reading child shares the lead's
   worktree and is read-only by instruction only. For a pipeline's declared
   review (§8.2) Archon checks that a child of the lead completed on the
   reviewer's model, which Aether records. What that child read and what
   it found are not in that record: they remain the lead's report.
5. **`delegate` takes no tool exposure or approval policy for the child,
   and copies none of its parent's instructions.** A child inherits its
   parent's `config`, so the orchestrator's children see what it sees, but
   its instructions are replaced by the ones `delegate` was passed, or by
   none. A rule therefore reaches a child only when its parent writes it
   there: the orchestrator's prompt (§4.4), the stage contract (§8.6) and
   the `adaptive` build prompt each require that, by instruction only.
   Hiding `integrate_workspace` relies on
   `ToolExposure` covering harness tools on the native MCP bridge (Aether
   documents that it does), Archon's mutating tools are refused to children
   by `archond` (§11), and the §7.4 watchdog is the backstop for models.
   Under the operator's `[approval] default`, a child's gated call waits for
   an approval in Aether like the orchestrator's own (item 9). A child also
   inherits the orchestrator's memory scope, not its repository's.
6. **Costs are reported or estimated, not billed**, and unknown cost is not
   zero. Budgets are best-effort floors, and Aether's `cost_budget` does not
   stop a turn already running.
7. **`wait_for` with native background jobs is not restart-durable.** Archon
   never depends on an agent staying attached. All progress is driven by
   persisted tasks and events.
8. **An explicit `scope` denies network by default.** `[network] default`
   applies only to roots created without a scope. Archon always passes a
   scope, so it copies `status.network_default` into it (§4.4): the
   orchestrator at each creation, a run node once per generation, and stage
   agents the narrower of their node's and the default when bound (§4.5). When network is denied, the `claude-code`
   engine runs each turn sandboxed, and no stage could fetch dependencies or
   reach a remote.
9. **Approvals have no Archon surface.** Stage agents gate nothing (§4.5).
   The orchestrator keeps the operator's `[approval] default` unless the
   global `[autonomy] default` has `tools = "yolo"` (§18.4). Under `"ask"`,
   its gated calls (web tools) wait for an approval in Aether, which
   `archon chat` does not show.
10. **Network-enabled `claude-code` agents load the operator's `~/.claude`
    settings and plugins.** Stage agents therefore see the operator's own MCP
    servers, for example GitHub write tools, besides the `aether` bridge.
    `ToolExposure` filters only the bridge, so the stage contract, not Aether,
    keeps stages off them. Archon's ship modes still decide what is pushed.
11. **Pausing an agent in Aether stops it at a turn boundary.** A turn that
    is already running completes, so `archon pause --hard` does not stop a
    stage mid-turn.
12. **A delegated child's workspace must lie under its parent's.** Aether
    compares the two paths as written for a shared workspace, and resolves
    links only for an isolated one. A symlink inside the parent's workspace
    therefore passes Aether's check for a shared child while leading
    anywhere on disk. Archon keeps no such link and resolves the paths
    itself: the watchdog cancels a helper of the orchestrator whose scope
    leads into a registered repository or the worktrees root (§4.4). A
    mirror is a directory of real files under the orchestrator's workspace,
    so a helper placed in one passes both checks; a change to a repository
    is an item.
13. **Aether cancels an agent's children with it, but not when its task
    completes or fails.** A stage agent that settles while a child still
    runs would leave the child working beside the next stage. Archon owes a
    `cancel` of every settled stage agent that could delegate and starts
    nothing else in that run before Aether holds no agent beneath it in a
    non-terminal state (§6.3). Aether's `cancel` is cooperative: its answer
    means the request was taken, not that an agent has stopped, so Archon
    reads the subtree back instead of trusting the answer. It relies on
    Aether applying a `cancel` of an already ended agent to its live
    descendants. Aether's cancellation is written to do so, but its protocol
    does not state it as a contract; if a later Aether refuses such a
    `cancel`, the run fails with a `Blocker` rather than going on, and if it
    takes the `cancel` without stopping them, the run waits and a `Blocker`
    names them.
    A replaced orchestrator's children are cancelled the same way (§16) but
    its successor does not wait for them to end. They hold no repository or
    worktree (item 4), `archond` refuses them every tool that changes Archon
    (§11), and their results go to the retired agent's inbox, so the worst
    overlap is a cancelled helper finishing a write in the scratch directory
    or in a mirror, which the next refresh checks out again (§13.3). Holding
    the operator's only interface on a helper that does not end would cost
    more than that.
14. **Aether's own limits apply to the orchestrator's children.**
    `[scheduler] max_concurrent_turns` is shared with stage agents, so many
    children slow stages down. `[scheduler] run_budget_usd`, if set in the
    Aether config Archon uses, is cumulative for a run and never ages out:
    the long-lived orchestrator's run eventually crosses it, and every
    `delegate` is then refused until `archon orchestrator restart` starts a
    new run. The dedicated instance's config is seeded from the operator's
    (§4.7), so the budget is set there whenever the operator set one.

## 18. Autonomy

How much Archon does on its own is set **per item** by five knobs. There are
no presets or profiles. The orchestrator picks each knob from the change's
risk and the operator's directives, within a ceiling that only the operator
can raise. The types are in `archon_core::autonomy`.

### 18.1 Knobs

| Knob | Values, least to most autonomous | Effect |
| --- | --- | --- |
| `review` | `strict`, `standard`, `none` | With no pipeline named, picks `strict-pr-review`, `default-pr-review` or `quick-pr`. A named pipeline that holds its own review (`adaptive`, `direct`; §8.2) runs at the level asked: three, one or no independent reviewers, verified. One with gated review and test stages fixes the level from them, and a `review` that contradicts it is refused. |
| `proposal` | `lavish`, `off` | `lavish`: a `propose` stage first writes `.archon/reports/proposal/index.html`, and the run waits for the operator (§18.3). The approved proposal, with the operator's note, goes into every later stage's prompt. |
| `report` | `before-pr`, `with-pr`, `off` | `with-pr`: a report stage before shipping writes `.archon/reports/change/index.html`. The page is published, the report goes into the PR body, and nothing waits. `before-pr` also waits for the operator before shipping (§18.3). |
| `merge` | `manual`, `auto` | `auto` sets `{{ship.auto_merge}}`: archond asks GitHub to merge the PR once its checks allow (§8.7), and a ship stage of a pipeline that has one merges its own. |
| `tools` | `guarded`, `yolo` | What the item's agents may use (§18.4). |

The defaults are `review = standard`, `proposal = off`, `report = off`,
`merge = manual` and `tools = guarded`. `[autonomy] default` in either
config file changes them (§9). The enqueue resolves the knobs once and
stores them on the item (`QueueItem.autonomy`), and a retry keeps them.
`proposal`, `report` and `merge` mean something only for a pipeline that
ships (`[pipeline.ship]` or a ship stage). Other pipelines run with `off`,
`off` and `manual`, and they refuse anything else. `tools` applies to every pipeline.

**Inserted stages.** When a knob needs a `propose` stage or a change report
and the pipeline declares none, `archon_pipeline::autonomy::augment` inserts
one: `propose` first, `report` last, right before the change ships (before
the ship stage of a pipeline that has one). Each inserted stage has
`fan_out = 1`, no gate, a 45-minute timeout and the shared prompt from
`pipelines/_shared/`. If `model_plan` leaves it out, it gets the first
choice of the pipeline's first stage. A pipeline that declares a `propose`
stage, or a `report` stage before the change ships, gets no duplicate, and
the knobs gate the declared stage. The run's frozen snapshot holds the
augmented pipeline, so a restart replays the same stages.

### 18.2 The ceiling

`AutonomyPolicy::resolve` builds the defaults and the ceiling of one
repository in three steps:

1. The global `[autonomy] max`, laid over the built-in ceiling. The built-in
   ceiling allows everything except `merge = auto`.
2. The registry's ceiling for the repository. It replaces the global ceiling
   knob by knob, because only the operator can set it:
   `archon repo set api --autonomy-max review=none,merge=auto`.
   `archon repo add --auto-merge` sets `merge=auto`, and
   `repo set --auto-merge true|false` changes only that knob.
3. The repository's `.archon/config.toml` `[autonomy]`. Its `max` and
   `default` can only lower.

A global default above the global ceiling is a config error. Defaults above
a repository's lower ceiling are clipped to it. The daemon checks every
enqueue against the ceiling, and it checks again when a run starts in case
the operator lowered it in the meantime. A refusal names what is allowed:

```
autonomy above the ceiling for repo api: merge=auto (allowed: manual); only the
operator can raise it (`[autonomy] max` in config.toml or `archon repo set --autonomy-max`)
```

Nothing that reaches Archon over MCP can raise a ceiling. `RepoUpdate` and a
`RepoAdd` with `auto_merge` are operator-only (§10), Archon writes no config
file, and directives never raise it (§18.5).

Store migration 2 carries over the old `auto_merge` flag. A repository that
had it gets the ceiling `merge=auto`. Existing items get knobs that match
their pipeline (`quick-pr` gets `review = none`, `strict-pr-review` gets
`strict`, anything else `standard`), plus `merge = auto` where their
repository had `auto_merge`. So queued and running items behave as before.

Store migration 3 keeps idempotent enqueues working across that upgrade. An
enqueue's key is matched against a fingerprint of the fields it resolved to,
and since migration 2 that fingerprint includes the knobs. Items enqueued
before migration 2 get their original fingerprint back, without knobs. Their
key then matches a retry that asks for no knob and agrees on every other
field, whatever knobs the retry resolves to; a retry that names a knob is a
different request and is refused.

### 18.3 Operator gates

A gate is the passed `propose` stage of an item with `proposal = lavish`, or
the passed change-report stage of an item with `report = before-pr`.

1. The `RunDriver` returns `AwaitOperator`. The run moves to
   `awaiting_operator{stage_id, attempt, subject}`, and no agent runs for it.
2. `archond` records an `OperatorGate` row keyed `(run, stage_id, attempt)`,
   so a replayed wait finds the same gate. It publishes the stage's page with
   Lavish, or names the file path without it, and asks a `Question`:

   ```
   Approve the direction for qi_… "Add a greeting flag"? Answer yes (a note after it
   goes along), reject to cancel the item, or give feedback and the proposal is redone
   with it.
   Page: http://127.0.0.1:4455/session/…
   <the stage's summary>
   ```

   The options are `yes` and `reject`. In chat the operator types
   `/a 3 yes`, `/a 3 <feedback>` or `/a 3 reject`; `archon answer 3 …`
   works the same, because `3` is the question's permanent number (§5.2).
   If the gate was decided elsewhere in the meantime (step 4), `archon
   answer 3` is refused and never decides a different gate. It prints the
   number, id and first line of the question it answered. With the question,
   `archond` pushes a `gate_opened` event (§5.4), from which the orchestrator
   restates the gate in its Aether chat, where the operator can answer with
   a message that is exactly `/answer 3 …` (§5.2, dedicated mode).
3. `GateDecision::from_answer` reads the answer. `yes`, `y`, `approve`,
   `ok` or `lgtm` approves, and any text after the word is a note.
   `reject`, `cancel` or a bare `no` rejects, and text after `reject` is the
   reason. Anything else asks for a revision, with the whole answer as the
   feedback.
4. **Lavish.** Prompts typed on the gate's page decide it the same way and
   close its chat question once the decision is acted on. Whichever answer
   comes first wins. Neither answer reaches the orchestrator. Either way, a
   decision that fails leaves the question open to answer again (§5.2).
5. **Approve** starts the next stage. **Revise** re-runs the `propose` stage
   with the feedback, or for a change report the pipeline's first work
   stage, and every stage after it runs again. The feedback reaches that
   stage even if its prompt has no `{{feedback}}` (§6.4). A revision is a
   fresh round of work: each stage it re-runs gets its full `on_fail`
   fix-loop budget again. After 5 revisions of one gate the run fails, which
   bounds the total. **Reject** cancels the item with the reason, and the
   orchestrator hears of it as `item_finished`.

The gate and its decision are persisted before the run moves on. After a
restart the driver is rebuilt with the recorded decisions. An open gate is
not asked twice. A gate whose answer was recorded but whose decision was
not is asked again. When a run finishes or is cancelled, its open gates are
withdrawn and their questions closed, with their feed answers, in the same
transaction. At startup, `archond` also closes any question an older version
left open on a withdrawn gate. A "Reopen it?" question about a gate's page
closes with the gate, right after the decision or the finish (§14).

### 18.4 Tool exposure

| Agent | `tools = guarded` | `tools = yolo` |
| --- | --- | --- |
| Orchestrator | The operator's `[approval] default`. Hides `integrate_workspace`, and `delegate` while Aether names no callers (§11). | `approval: {"require_for": []}`. Hides the same tools. |
| Its delegated children | Inherit its approval policy and tool exposure. `archond` refuses them Archon's mutating tools (§11). | The same, so nothing waits. |
| Stage agents | Approval gates nothing (they are headless, §4.5). A leaf, unless the stage sets `allow_delegation` (`leaf = false`). Exposes `message_orchestrator`, hides the other Archon tools and Aether's schedule tools. | Approval gates nothing. May delegate (`leaf = false`). Exposes `message_orchestrator` and Archon's read-only tools; hides Aether's schedule tools. |
| Their subagents | Inherit their parent's approval policy and tool exposure. | The same, so nothing waits. |

A stage agent's `tools` knob is the item's. The orchestrator serves every
item, so its knob is the global `[autonomy] default` `tools`. A change
applies from the orchestrator's next generation, and
`archon orchestrator restart` applies it at once.

What stays hidden, and why:

- **`integrate_workspace`, from the orchestrator.** It copies a delegated
  child's edits into the parent's workspace. A change to a repository must
  be an item, so that the worktree, the pipeline's gates, the ship mode, the
  caps and the autonomy ceiling apply; a child's edits would bypass all of
  them.
  `delegate` itself is exposed: the orchestrator uses it for work that
  changes no repository (§4.4). Of the limits an item has, the model policy
  and the daily and total budgets reach such a child; the caps and the
  autonomy knobs do not.
- **Archon's mutating tools, from stage agents.** `enqueue`, `cancel_item`,
  `pause`, `notify`, `set_directive` and the rest act for the orchestrator.
  A stage could otherwise queue work, cancel its own item or change
  directives outside the orchestrator's decisions and the operator's chat.
  The orchestrator's children are not hidden from them, because `delegate`
  cannot narrow tools; `archond` refuses their calls instead (§11).
  Under `yolo`, the read-only tools (`get_item`, `list_queue`,
  `list_pipelines`, …) are exposed. `message_orchestrator` is the mutation
  exception in every mode: it records attributed news without changing
  queue decisions (§5.5).
- **Aether's `delegate`, from `guarded` stage agents whose stage does not
  set `allow_delegation`.** They are created as leaves. A stage that sets
  `allow_delegation = true` gets `leaf = false` under `guarded` too, and
  `tools = yolo` lets every stage agent delegate. The flag changes nothing
  else: which Archon tools the agent sees still follows the item's `tools`.
  Aether's isolated delegated workspaces are managed Git worktrees that
  share the repository's Git objects. They materialise the tracked files
  and the untracked files that are not ignored; ignored caches and build
  outputs are left out. What a child builds in its worktree still costs
  disk.
- **Aether's own tools, from a stage agent that can delegate.** `leaf =
  false` also brings Aether's orchestration tools, to guarded stage agents
  for the first time. The same list applies under `guarded` and `yolo`, and
  a child inherits it:
  - Hidden: `create_schedule`, `update_schedule` and `list_schedules`. A
    stage is bounded work that Archon settles and stops; a schedule would
    wake an agent after its stage ended, outside every cap and budget of
    the item.
  - Hidden where the pipeline's workspace is `repo-readonly`:
    `integrate_workspace`. Nothing is integrated into a read-only workspace,
    and its delegation rules offer no isolated children (§8.6).
  - Kept: `delegate`; `integrate_workspace` in a writable workspace, which
    is how an isolated child's accepted edits reach the lead's worktree;
    `list_agents`, `list_tasks`, `read_task` and `cancel_agent`, with which
    a lead follows, reads and stops its own children; and `list_models`,
    which shows what is connected, while the delegation rules state what is
    allowed and the watchdog enforces it (§7.4). Aether lets `cancel_agent`
    act only on the caller's own delegated descendants; the read tools are
    Aether's listings, which Archon does not narrow.

### 18.5 Operator directives

A directive is a standing instruction in plain language, such as "fully
autonomous today, auto-merge everything" or "propose first for anything in
auth/".

- The operator records one with `archon autonomy direct <text…> [--hours N]`.
  When the operator says one in chat, or in a GitHub request that Aether
  delivered with operator standing (`docs/github-watcher.md`), the
  orchestrator records it with `set_directive`. Each directive gets a `dir_…` id and may expire after 1 to
  720 hours. A directive is at most 500 characters, and at most 20 are in
  force at once.
- The first trigger of an orchestrator generation carries the directives
  in force, later ones what changed, and all of them again after a
  restart, a revival or a compacted context (§5.4). `autonomy_policy`
  lists them at any time, so they survive whatever the orchestrator's
  context loses.
- Directives steer the orchestrator's choice of knobs within the ceiling and
  never raise it. "Auto-merge everything" under a `merge = manual` ceiling
  gets the refusal, and the orchestrator tells the operator which setting to
  change.
- `archon autonomy clear <id>` or `--all`, or the `clear_directive` tool,
  ends directives early. Expired ones are pruned. Each change adds a
  `System` note to the feed.

`archon autonomy [--repo R]` shows the defaults, the ceiling and the
directives. `archon status` adds the global policy and the gates that are
waiting, and `archon queue show` shows an item's knobs and gates. The
orchestrator states the knobs it chose, with a reason, in the `decision` it
notifies for each item.


## 19. Learning

Learning is token-free: it records what runs did and what was later said
about them, and uses that to fill model plans and to hand agents short
lessons. Only distillation spends, through a budgeted item.

**Outcomes.** `store/outcomes` stores one outcome per finished pipeline
run, written as the run finishes (`learned::finished`, from
`executor/finish.rs`): its result, failure class, stage attempts (one
grouped observation per model and effort, whose costs include the agents
below each stage lead, with a reused agent's ledger charges counted once),
cost, wall time, diff size, corrections, and `merged`, empty until
observed. `capture::failure_class` reads the class from the run's error:
`escalated` and `blocked` from the `ESCALATE:` and `BLOCKED:` prefixes
(§8.6), else what the text names. Distillation items produce no outcome.
The learning worker (`learned::run`) imports runs that finished before the
update in bounded, resumable batches; migration V10 creates only empty
tables, so no migration scans historical runs.

**Corrections** (`archon_core::outcome::Correction`) are appended
idempotently, kept even before the run's outcome exists, and rescore
`lesson_uses`, so a success that was corrected is no win:

- `retry_after_done`: a retry of a succeeded item, written on its
  `Succeeded → Queued` transition (`store/items.rs`);
- `gate_rejected`: a gate the operator rejected (`store/gates.rs`);
- `steered`: a steer of a running or revived run (`steer/audit.rs::record`,
  `steer/shape.rs::correct_shaped`);
- `operator_contradiction`: an operator message naming a full `qi_…` id
  with "wrong", "incorrect", "not fixed", "still fails" or "regression"
  (`learned::operator_message`);
- `pr_closed` and `changes_requested`, from GitHub.
  `executor/pull_requests.rs::observe`, run by the coordination worker
  every 10 minutes, reads each succeeded outcome of the last 14 days that
  is not settled and whose run archond delivered as a pull request, with
  `gh pr view --json state,reviewDecision,…`. `MERGED` sets `merged`
  true; `CLOSED` sets it false and adds `pr_closed`; an open one whose
  review decision is `CHANGES_REQUESTED` gets `changes_requested` once. A
  settled outcome is not read again. A `gh` failure is logged and the pull
  request read again at the next pass; a store failure ends the worker,
  and with it the daemon, as in every worker.

**Learned defaults.** `knobs::fill_plan` calls
`learned::default_plan(store, repo, spec, policy)` with the item's
effective model policy, for the stages its plan leaves out. Samples come
from `store/model_evidence` by repository, pipeline, stage and, when the
item's `diff:small`, `diff:medium` or `diff:large` tag gives it, diff size;
repository evidence falls back to global evidence when no choice in it has
five runs. A choice qualifies with five runs, and among those within five
percentage points of the best pass rate the cheapest median cost wins. The
item's id decides whether it is among the `explore_rate` (at most 0.1) of
items that try the adjacent cheaper choice. An explicit choice is never
replaced, and the policy is checked before a default is used.
`learned::plan_for_item` stores the reason for each choice per item, which
`archon lessons why` shows. `handlers/items.rs::filled_plan` records the
static default the plan would have had without learning
(`Store::record_static_plan`); `estimated_model_saving` compares medians
only once that baseline has five samples, and reports unknown before.

`[learning]` is mirrored into the store's `kv` (`learning/config`) by
`Store::configure_learning` where the daemon builds its context and in the
handlers that read it, so that store functions see it. `enabled = false`
stops outcome writes, learned defaults, lesson use and scoring, hints and
distillation; what was recorded stays readable.

**Lessons.** `store/lessons` owns normalization, scope limits,
deduplication, operator edits, Wilson confidence and the 30-day decay of
unconfirmed lessons. Five scored uses whose decayed confidence falls below
0.3 retract a lesson; a pinned one never decays or retracts. An operator's
lesson is confirmed at once, the orchestrator's (`record_lesson`) is active
at once, and a distilled one is `proposed` until `archon lessons accept`.

- A stage agent gets at most six lessons, in a section of at most 1,200
  bytes, preferring its repository and pipeline (`learned::lessons_for_run`
  at spawn). The text is frozen per run in `learning/uses/<run>` and joined
  to outcomes through `lesson_uses`.
- A new orchestrator gets `learned::lessons_for_orchestrator` appended to
  its instructions under `## Learned observations` (§4.4) by
  `supervisor::create_generation`. The selection is frozen in
  `learning/orchestrator/<generation>`, so a retried create sends the same
  text.
- A failed item's `item_finished` carries `lesson_hint`: the result,
  failure class and corrections of its newest outcome, and the ask to
  `record_lesson` a reusable practice.

**Distillation.** Every `distill_every` new outcomes of a repository,
`learned::maintenance` queues one idempotent, low-priority `errand` item
with a $1 budget under the normal policy, accounting and caps. Its brief
quotes the observations as untrusted and asks (`distill::DISTILL_ASK`) for
lessons and an optional `pipeline_proposal` in Archon's pipeline format; one
that changes a repository needs `workspace = "worktree"`, no ship stage and
`[pipeline.ship] stage` naming the last stage that commits.
`archon_pipeline::proposals::stage` validates it as any user pipeline,
archond's delivery rules (§8.7) included, and writes it to
`<user pipelines>/<name>.proposed/`, which discovery skips.
`proposals::accept` (`archon pipelines accept`) validates it again and
renames it into place, never over an existing pipeline.
