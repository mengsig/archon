# Configuration

`~/.config/archon/config.toml` on Linux (or `$XDG_CONFIG_HOME/archon/config.toml`),
`~/Library/Application Support/archon/config.toml` on macOS.
`ARCHON_CONFIG_DIR` overrides the directory on either platform; set it
together with `ARCHON_DATA_DIR` and `ARCHON_SOCKET`
([why](guide.md#which-commands-start-archond)). Every field
is optional, and so is the file: `archon setup` writes one with every key
commented out when there is none. Unknown keys are an error, so a typo
cannot silently disable a limit. Only `config.toml` is read. An
`archon.toml` from before 1.0 lying beside it is ignored: `archon doctor`
warns about it, and `archond` says so once in the feed when it starts.

You do not list models or model tiers anywhere. With no `[models]` section
Archon may use every model Aether reports ready (`archon aether models`),
and the orchestrator picks a model and effort for each stage of each item.
`[models]` only narrows that choice. Aether places each model in a tier
(`fast`, `balanced`, `strong` or `frontier`), so a plan may ask for a tier
instead of a model and a new model release needs no Archon change. Defaults
never pick above `max_default_tier`; set it to `"frontier"` to let them.

A tier without a provider runs on the orchestrator's provider, unless
`provider_weights` is set. Then each item is given one provider, picked by
weight from a hash of its id, and all of its tier-only choices run there,
through every stage and retry; `get_item` names it. A provider whose best
allowed model is below a stage's floor (the tier asked for, or the stage's
`min_tier`) is skipped for that stage; with none left, or only ones weighted
0, the choice is refused, naming each provider's best tier. A provider Aether
lists but the table leaves out gets `default_provider_weight`. A repository's
entries and default override the global ones; the rest of the table stays.
An exact `model` or a named `provider` always wins. `archon cost --by provider`
and `model_evidence` show the share each provider got.

With weights set, Archon also reads each provider's usage from Aether (at most
every 30 seconds) and routes by weight × headroom. A window's headroom is what
is left above the reserve per second until it resets, over the even rate
`100 / window length`, clamped to 0..2; a provider's is its tightest window's.
So 30% left resetting in 10 minutes outscores 30% left for five days, and a
nearly spent provider gets almost nothing whatever its weight. `reserve_percent`
holds back part of every window for the orchestrator (on its own provider) and
for the providers named in `interactive_providers`; stages never spend it. A
provider with no report, a stale one, or a reset already passed keeps its plain
weight. An item keeps its provider until that one has no headroom left; its
stages not yet started then move to the best provider that meets their floor,
and `get_item` records the switch. While every provider a stage may use is out,
a queued item waits (`provider_quota`, naming the earliest reset) and a running
one's next stage stays pending (`get_item` says so); each starts once a window
resets, and a stage is never sent below its floor. A kept agent resuming a loop
stays on its provider. `status` and `queue_status`
show each provider's headroom, reserve and next reset. Reserves and interactive
providers are global settings only.

```toml
[models]                       # optional: leave it out to allow every ready model
allow = ["claude-code/claude-opus-5-5", "codex/gpt-6.1-sol"]
max_effort = "high"
# max_default_tier = "strong"  # the highest tier a default picks (the default)
# provider_weights = { "claude-code" = 7, codex = 3 }   # route tier-only choices by share
# default_provider_weight = 1  # a provider the table leaves out (the default; 0 = never)
# reserve_percent = { orchestrator = 15, interactive = 10 }   # quota stages leave (the defaults)
# interactive_providers = ["claude-code"]   # providers someone uses by hand; global only

[limits]
max_parallel_agents = 8        # the hard cap on running stage agents (the default)
max_per_repo = 2
budget_usd_daily = 50.0
# budgets_enabled = false      # no dollar budget is enforced at all; spend is still recorded
min_free_gb = 10               # nothing new starts below this much free disk; 0 = off
[limits.per_pipeline]
adaptive = 3

[worktrees]
auto_gc = true                 # archond collects finished runs' worktrees itself
cache_after = "1h"             # then deletes their build caches (git-ignored files)
gc_after = "7d"                # and removes worktrees without uncommitted changes
cache_dirs = ["target"]        # only these caches; unset = everything git ignores
item_cache = true              # each item builds in its own cargo target dir, deleted when it ends
lean_builds = true             # no incremental state, line-table debuginfo (dev and test)
shared_cache = false           # true: one cargo target dir per repository instead
shared_cache_after = "14d"     # idle shared caches go; never while a run is live
pressure_after = "1d"          # shorter ages while dispatch is held for disk space

[artifacts]                    # files stages publish; read by item id later
max_file_mb = 256
max_item_mb = 1024
keep_after = "14d"             # after the item settled; "0s" disables expiry

[storage]
retention_days = 14            # settled item reports and daemon log age; 0 disables age expiry
log_max_mb = 10                # rotate detached daemon logs by size; 0 disables size rotation
log_max_files = 3              # archives in addition to the active log; 0 disables count limit
```

Each item's cargo target directory (`<data>/cache/<repo>/items/<item>`) is kept
across its retries and deleted when the item reaches a final state; `archon
gc` sweeps ones whose item is final or gone. A repository sets `[cache]
item = false`, `shared = false` or `lean = false` in `<repo>/.archon/config.toml`
to opt out of the per-item directory, the shared one or the lean profile. Cargo
locks a shared target directory, so parallel builds of one profile queue on it.

The lean profile is a `.cargo/config.toml` archond writes above the
repository's worktrees: `[build] incremental = false` and `debug =
"line-tables-only"` for `[profile.dev]` and `[profile.test]`. A repository's
own `.cargo/config.toml` wins for the keys it sets, but archond's `[profile]`
beats the one in `Cargo.toml`: set `[cache] lean = false` to keep full
debuginfo. New work already holds below `[limits] min_free_gb`.

Filesystem ages retain the existing duration syntax. Set `gc_after`,
`cache_after`, `shared_cache_after` or `artifacts.keep_after` to `"0s"` to
disable that expiry, including pressure GC. `pressure_after = "0s"` disables
pressure acceleration. An explicit `archon gc --older-than 0s` still requests
immediate collection; dirty worktrees remain protected unless `--force` is
explicitly requested. `keep_on_success = false` still permits normal cleanup
immediately after shipping.

`[storage]` uses integer days, matching Aether's retention setting. It applies
to Archon's owned report/Lavish snapshots after both the item and report have
been quiet for that long. Live or recently settled items, unfinished or recent
runs, open reports, open gates, pending questions and stack fixes are protected.
Original source pages are never removed. The existing store prune leaves an
item's owned snapshot to filesystem GC; metadata and orphan cleanup retain
their existing 7-day policy. Disabling `worktrees.auto_gc` also disables the
periodic report pass; online `archon gc` still applies it. Offline GC collects
workspaces, caches and artifacts only, leaving reports for the daemon.

Detached `archond.log` rotates while the daemon runs, keeping the active file
and three archives by default. Rotation applies size, archive count and age
limits independently; `0` disables each corresponding limit. Foreground daemon
output remains on its configured stdout/stderr. Storage config validation
allows `retention_days` 0..36500, `log_max_mb` 0..1048576 and `log_max_files`
0..1000. SQLite rows, the spend ledger and 90-day evidence windows are unchanged.

`archon status` and MCP `queue_status` show allocated bytes by storage kind,
including database WAL/SHM and the startup log beside the socket, plus the last
GC that freed data. Filesystem readings are cached for up to one minute and
invalidated after GC. Unknown sizes report the read error. Symlinks are counted
without following them. Bench results live in independently selected output
roots and are not swept or measured by the daemon.


To run Archon on your own `aetherd` instead, set `[aether] mode = "shared"`.
In shared mode, `archon setup` adds the `archon` MCP server to your
`~/.config/aether/config.toml`, and every agent of that daemon then sees
Archon's tools. The ones that change Archon answer only the current
orchestrator, which needs `send_caller_meta = true` on that registration
so that Aether names the calling agent. `archon setup` writes the line,
and never edits a registration that is already there: if yours lacks it,
`archon doctor` fails and says so, and the
[guide](guide.md#after-upgrading) has the fix. Answering a gate or
question with `/answer <n> …` in the orchestrator's chat is off in shared
mode, because a client holding that daemon's `[listen]` token is the same
`user` actor as you; see the
[guide](guide.md#answering-in-the-orchestrators-chat).

`[aether] host_context = "full"` is the default. Opt in to lean context for
new agents with:

```toml
[aether]
host_context = "lean"
```

On Claude Code, lean context removes the Skill tool and host MCP servers
outside Aether. Aether's MCP server (including Archon's tools), built-in
tools, CLAUDE.md, hooks and settings remain available. Codex ignores this
setting. Archon applies it to the orchestrator, run nodes and stage agents;
Aether's `delegate` tool inherits it for helpers unless explicitly overridden.
It is fixed at agent creation: existing and resumed agents keep their context.
Restart the orchestrator to change its context. There is no per-stage override.

Archon sends lean only when Aether advertises the `host_context` feature;
protocol 17 alone does not imply support. An older Aether gets full context,
with a daemon log warning and a warning in `archon doctor`. Pending lean
requests fall back to full if support disappears, with one warning per
changed stage or node. If Aether already accepted the lean request and
rejects its changed replay, Archon fails the stage visibly or advances the
run node to a new generation. Existing agents keep their original context.
Doctor shows configured and effective context for new agents. Stage records
retain the effective policy, and `model-evidence` groups lean and full
separately for Claude Code; Codex stages record full. Full stays the default
pending a quality benchmark.

A relative path in the config, such as `[worktrees] root`, is relative to
the directory holding `config.toml`, and `~/` expands to your home
directory. The daemon runs in Archon's data directory, so it never depends
on where you started it.

Each repository can lower these limits in `<repo>/.archon/config.toml`. The
full reference is in [`docs/ARCHITECTURE.md` §9](ARCHITECTURE.md#9-configuration).

The orchestrator reads your code in mirrors: for every registered
repository, `archond` keeps a clone at `<data>/orchestrator/repos/<name>`,
checked out at the base branch, with no remote, and refreshes it when an
item finishes and when you write. A mirror shares the repository's objects
through hard links when both are on one filesystem, so it costs about one
more checkout; on another filesystem it is a full copy. A repository whose
own `[models]` refuses the orchestrator's model gets no mirror.

`archond` posts every finished item to your feed itself and wakes the
orchestrator for a success, a failure, a blocker, an item tagged `notify`
or one a GitHub thread asked for; every wake is a whole orchestrator turn
that you pay for. A routine success waits, up to five minutes, while other
items are queued or running, so one wake reports them together. The success
wake is what shows a finished item in the terminal UI. If you follow Archon
in `archon chat` instead, set
`wake_on_success = false` under `[orchestrator]`: a routine success then
reaches the orchestrator with its next turn
([guide](guide.md#when-the-orchestrator-is-woken)).

The helper agents the orchestrator delegates to
([guide](guide.md#what-the-orchestrator-does-without-queueing)) work in a
mirror or in a scratch directory, never in your checkout. A helper is held
to `[models]` narrowed by the `[models]` of the repository whose mirror it
reads, and to `[models]` alone when it reads none. It may delegate once
more, and no deeper ([`max_delegation_depth`](#structural-limits)).
Helpers' spend counts toward the daily and total
budgets. `max_parallel_agents` and the other caps count stage agents
only, so the orchestrator is told to keep its helpers within them. If your
Aether config sets `[scheduler] run_budget_usd`, Archon's instance inherits
it, and it is cumulative: once the orchestrator's run has spent it, Aether
refuses every delegation until `archon orchestrator restart`.

## Operator authority

```toml
[authority]
github_may_loosen = true   # default: a verified GitHub operator may approve a loosening change
```

With `false`, a request from GitHub can only tighten settings; one that would
loosen them is refused instead of asked. Chat is unaffected. See
[operator authority](operator-authority.md).

## Structural limits

These `[limits]` keys bound shapes rather than spend. Each default is the
value earlier releases fixed, and each has a ceiling the daemon will not
go past:

```toml
[limits]
max_delegation_depth = 2         # 1..=4
max_stage_timeout_minutes = 1440 # 1..=43200
max_agent_messages = 20          # 1..=200
initiative_members = 16          # 1..=64
initiative_contract_bytes = 2048 # 1..=16384
```

- `max_delegation_depth`: how many delegations below a stage agent or the
  orchestrator an agent may be. 2 lets a child delegate once more; 1 makes
  every child a leaf. Every level adds agents that run outside
  `max_parallel_agents`. The orchestrator's prompt and each delegating
  stage's contract state the configured depth.
- `max_stage_timeout_minutes`: the longest per-stage timeout an item may
  ask for with `stage_timeouts` at `enqueue`, `update_item` or `steer_item`. A pipeline's
  own `timeout_minutes` is not capped by it.
- `max_agent_messages`: the `message_orchestrator` calls one stage agent
  may make in its lifetime, and the messages one run node's inbox may
  relay. Each message is an orchestrator turn.
- `initiative_members` and `initiative_contract_bytes`: an initiative's
  size. The contract is in every member's stage contract, and is sent
  whole to running members whenever it changes.

Others stay fixed. The pipeline format's own bounds (32 stages, `fan_out`
16, `max_loops` 10) are the same everywhere, so a pipeline that loads on
one installation loads on all ([pipelines](PIPELINES.md)). The git-identity
rules, workspace placement, model policy, budgets, review evidence and
gates are protections, not sizes, and no configuration turns them off.

## Learning

```toml
[learning]
enabled = true
distill_every = 20
distill_daily_budget_usd = 3.0
explore_rate = 0.1
```

Learning keeps an outcome for every finished run: its result, its stage
attempts with what their delegated agents cost, and the corrections it drew
later, among them what GitHub says of the pull request `archond` delivered
(merged, closed, or sent back by a reviewer; read every 10 minutes for 14
days). Runs that finished before you updated are imported in small batches
in the background; their diff sizes stay unknown.

Omitted stages use conservative Wilson quality bounds, with a small cost penalty,
within the model policy. Evidence is grouped by repository, pipeline content revision,
review level, stage, task class (`task:<class>`, otherwise `general`) and known diff
size (`diff:small`, `diff:medium`, `diff:large`). Explicit choices stay explicit.
`explore_rate`, from 0 to 0.1, bounds experiments with ready, allowed models from
Aether's catalog, including models with no history. The item ID makes the choice
replayable. Choices with at least five samples whose upper quality bound falls below
the incumbent's lower bound are excluded from exploration. `lessons why` explains
the bounds, sample size and context; five wins are not proof of equivalence.

`distill_every` (1..100) counts new outcomes per repository before a $1 errand.
At most one distillation is queued or running per repository. Across repositories,
`distill_daily_budget_usd` reserves the full errand budgets per UTC creation day;
the default is $3, and 0 disables new distillations. With `budgets_enabled = false`
a positive reservation does not limit them; 0 still disables them. Imported history informs routing
and statistics but is never distilled, avoiding upgrade spending bursts.
Proposed lessons and pipelines wait for operator acceptance; inspect pipelines with
`archon pipelines proposals [name]` before `archon pipelines accept <name>`.

`enabled = false` stops new learning and cancels queued distillation errands on the
next maintenance pass. Recorded evidence stays readable, including offline lessons
commands. Config is loaded into daemon memory; restart after editing it.
Outcome costs are provisional at finish and read from the current spend ledger,
including later provider settlement. Missing outcomes are reconciled in recurring
bounded scans; malformed history gets a diagnostic skip marker instead of blocking
later runs.

## Disk use

Every item that works on a repository runs in its own git worktree, by
default under `~/.local/share/archon/worktrees/<repo>/<item-id>` on Linux
(`[worktrees] root` moves it), and each role of a team stage gets one more.
A worktree is a separate checkout, so it has its own build directory and
shares none with your checkout or with other items. For a large Rust project
one item's `target/` can reach tens of gigabytes, and several items running
in parallel multiply that.

Archon gives the space back in steps, and stops starting work before the
disk is full:

| Setting | Default | What it does |
| --- | --- | --- |
| `[worktrees] auto_gc` | `true` | `archond` collects by itself: when it starts, every 30 minutes, and when low disk space holds dispatch. `false` leaves collecting to `archon gc`. |
| `[worktrees] cache_after` | `"1h"` | This long after an item's last run finished, the build caches in its worktree are deleted. Source files, uncommitted changes and commits stay. |
| `[worktrees] cache_dirs` | unset | What counts as a build cache. Unset: every file git ignores in the worktree. `["target"]`: only ignored files under `target/`. |
| `[worktrees] gc_after` | `"7d"` | This long after an item's last run finished, its worktree is removed. A worktree with uncommitted changes is kept; [three kinds of directory](#what-is-deleted-with-uncommitted-work) are not. |
| `[limits] min_free_gb` | `10` | While the data directory's or the worktree root's filesystem has less free space than this, in GiB, no item and no stage agent starts; running agents finish. `0` turns the check off. |

Ages are a number and a unit: `90s`, `30m`, `12h`, `7d`, `2w`. Apart from
this, the worktree of a run that shipped in the `pr` ship mode is removed
as soon as it has shipped, unless `[worktrees] keep_on_success = true`.
`archon status` shows the free space and whether dispatch is held.

`archon gc` runs the same collection now and prints what it removed and what
it skipped:

- `--older-than <age>` removes worktrees that long after their item's last
  run finished, instead of `gc_after`.
- `--force` also removes worktrees with uncommitted changes, which are then
  lost. It is not what guards the directories listed below: those go
  without it.
- `--offline` collects without `archond`, which must not be running, for a
  disk too full to start it. Without the flag, `archon gc` does the same
  when no `archond` answers.

It removes build caches, the worktrees and scratch directories of finished
items, and what a fresh retry or a team run set aside. It never touches an
item that is still queued or running, your own checkout of the repository,
any branch or commit, or the published copies of reports.

### What is deleted with uncommitted work

Keeping uncommitted changes protects the worktree an item's agents work
in, and each role's worktree of a team run. These directories are deleted
whole, with whatever is in them, with or without `--force`:

- What was set aside: the earlier worktree that `archon queue retry --fresh`
  moved out of the way, and a role's worktree that a team run moved aside.
  Each goes `gc_after` (or `--older-than`) after the item's last run
  finished. Before that, once `cache_after` has passed, only the
  `cache_dirs` directories in it are deleted, and nothing when `cache_dirs`
  is unset.
- The scratch directory of an item that ran without a repository, at the
  same age.
- A snapshot that a read-only stage left behind, as soon as `cache_after`
  has passed, even when it has changes or is locked. A snapshot is
  disposable: normally it is removed when its run ends.

Commits and branches are never deleted. Copy out anything else you want to
keep from a directory that was set aside. The rules in full are in
[`docs/ARCHITECTURE.md` §13.2](ARCHITECTURE.md#132-worktree-gc).

If the disk still fills:

- Run fewer agents at once: lower `[limits] max_parallel_agents` (8 by
  default) or `max_per_repo`, or until the next restart
  `archon queue caps --max-parallel-agents 2`.
- Shorten `cache_after`, for example to `"15m"`, so finished items give
  their build directories back sooner.
- Make each build directory smaller. In a Rust project, start with the
  debug information of the dev profile: `debug = "line-tables-only"` or
  `debug = false` under `[profile.dev]` in the project's `Cargo.toml`.

## Select an available model

The default orchestrator uses `claude-code/claude-opus-5-5` at `high` effort.
If you are already signed in to Claude Code or Codex, neither Archon nor Aether needs a separate login for that engine.
After setup, `archon aether models` lists the models your Aether installation
actually exposes: for example `claude-code/claude-opus-5-5`,
`claude-code/claude-sonnet-5-5`, `claude-code/claude-haiku-5-5`,
`claude-code/claude-fable-5-1`, `codex/gpt-6-astra`, `codex/gpt-6.1-sol`
and `codex/gpt-6-luna`. The orchestrator's own model is the one model you
set, because it must be known before any agent runs. To use Codex alone,
set `[orchestrator] model` to an available `codex/…` entry before starting
Archon. If you do set `[models] allow`, include the orchestrator's model as
well as every model the stages may use.

```toml
[orchestrator]
model = "codex/gpt-6.1-sol"    # any entry from `archon aether models`
effort = "high"
```

Replace `codex/your-available-model` in both examples with an entry from
`archon aether models`, or remove it from the allowlist if you only use Claude.
Account access and the installed engine determine model availability.
Config is loaded when archond starts; restart it after edits when it is safe
to interrupt your work. `archon queue caps` changes saved operator caps without a
restart; these overrides last only until the daemon restarts.


## Saved operator settings

`archon settings [--repo NAME]` shows the stored overlay and each effective
value's source. `--set JSON` replaces that scope's overlay; `--reset` clears it.
The same controls are available through verified TUI/GitHub authority and the
orchestrator's `request_authority`; its read-only `settings` tool reads them.
See [operator authority](operator-authority.md) for commands and confirmations.

The order is config.toml, global operator overlay, repository registry and
operator overlay, then the repository file's autonomy limits (only lowering).
Global overlays support autonomy defaults/ceiling, all caps, budget switches,
and a default pipeline. Repository overlays support autonomy defaults/ceiling,
the budget switch, per-item budget, max_per_repo and default pipeline. Other repository caps
and full-auto are refused pending their enforcement implementation. Existing
items retain their saved autonomy; change an item explicitly to update it.

`archon queue caps` now saves its global override across daemon restarts.
`archon settings --reset` clears that overlay, including its other overrides.
