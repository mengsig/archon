# Configuration

`~/.config/archon/config.toml` on Linux (or `$XDG_CONFIG_HOME/archon/config.toml`),
`~/Library/Application Support/archon/config.toml` on macOS.
`ARCHON_CONFIG_DIR` overrides the directory on either platform. Every field
is optional, and so is the file: `archon setup` writes one with every key
commented out when there is none. Unknown keys are an error, so a typo
cannot silently disable a limit. Only `config.toml` is read. An
`archon.toml` from before 1.0 lying beside it is ignored: `archon doctor`
warns about it, and `archond` says so once in the feed when it starts.

You do not list models or model tiers anywhere. With no `[models]` section
Archon may use every model Aether reports ready (`archon aether models`),
and the orchestrator picks a model and effort for each stage of each item.
`[models]` only narrows that choice.

```toml
[models]                       # optional: leave it out to allow every ready model
allow = ["claude-code/claude-opus-5-5", "codex/gpt-6.1-sol"]
max_effort = "high"

[limits]
max_parallel_agents = 3        # the hard cap on running stage agents
max_per_repo = 2
budget_usd_daily = 50.0
min_free_gb = 10               # nothing new starts below this much free disk; 0 = off
[limits.per_pipeline]
strict-pr-review = 3

[worktrees]
auto_gc = true                 # archond collects finished runs' worktrees itself
cache_after = "1h"             # then deletes their build caches (git-ignored files)
gc_after = "7d"                # and removes worktrees without uncommitted changes
cache_dirs = ["target"]        # only these caches; unset = everything git ignores
```

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

A relative path in the config, such as `[worktrees] root`, is relative to
the directory holding `config.toml`, and `~/` expands to your home
directory. The daemon runs in Archon's data directory, so it never depends
on where you started it.

Each repository can lower these limits in `<repo>/.archon/config.toml`. The
full reference is in [`docs/ARCHITECTURE.md` §9](ARCHITECTURE.md#9-configuration).

The helper agents the orchestrator delegates to
([guide](guide.md#what-the-orchestrator-does-without-queueing)) work in no
repository. They are held to `[models]` narrowed by every registered
repository's own `[models]`, and their spend counts toward the daily and
total budgets. `max_parallel_agents` and the other caps count stage agents
only, so the orchestrator is told to keep its helpers within them. If your
Aether config sets `[scheduler] run_budget_usd`, Archon's instance inherits
it, and it is cumulative: once the orchestrator's run has spent it, Aether
refuses every delegation until `archon orchestrator restart`.

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

- Run fewer agents at once: lower `[limits] max_parallel_agents` (3 by
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
to interrupt your work. `archon queue caps` changes runtime caps without a
restart; these overrides last only until the daemon restarts.
