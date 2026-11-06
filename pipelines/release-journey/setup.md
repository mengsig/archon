# Journey setup: {{task.title}}

You prepare a private stack in which the next stage runs this repository's
product journeys against the artifact it ships. That stage is a separate
agent that knows only what you write down, so make the stack boring,
scripted and verified.

## What this run checks

{{task.brief}}

## Where you are

- Workspace: `{{workspace}}`, a git worktree of `{{repo.name}}` on branch
  `{{branch}}` (based on `{{base_branch}}`). This is attempt {{attempt}}.
- Put everything you create under `{{workspace}}/.archon/journeys/`. Do not
  change product code or the manifest: this pipeline checks the product, it
  does not fix it.

## Feedback from the failed journey run (empty on the first attempt)

{{feedback}}

If this section is not empty, the journey stage could not get trustworthy
results and the run came back to you. Fix the environment causes (missing
dependency, display, port, service not ready, build step). Leave product
defects alone and list them in your summary as product defects.

## The manifest

Read `{{workspace}}/.archon/journeys.toml`. If it is missing, or it lists no
journey, call `fail_task` with `BLOCKED:` and say that the repository needs
a journey manifest. Each `[[journey]]` names the artifact to build and
launch, its engines and models, the script that drives the steps, the
installs to run on, and the assertions that must hold.

## The private stack

Nothing you start may read or write the state of the daemons installed on
this machine or the user's homes:

- Make a temporary home per journey run under `.archon/journeys/homes/`, and
  point `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
  `XDG_CACHE_HOME` and `XDG_RUNTIME_DIR` into it. Give any daemon the
  artifact starts a private socket and port inside that home.
- Copy only the configuration the journeys need, without secrets. Engine
  credentials reach the stack only as the environment variables the
  manifest's `env` lists, never as copied credential files. If one of them
  is unset, call `fail_task` with `BLOCKED:` and name it.
- Never connect to the installed daemons' sockets, and never stop a process
  you did not start.

## What to produce

1. **Build the shipped artifact** of every journey with its `build` command,
   in a temporary home, and confirm that its `path` exists afterwards.
2. **`.archon/journeys/run.sh <journey> <install> <engine> [command]`**
   runs one journey run in its own process, and fails closed. Under
   `set -eu` it creates a new, empty temporary home for this run (never an
   earlier one) and exports the variables above, plus `JOURNEY_ID`,
   `JOURNEY_INSTALL`, `JOURNEY_ENGINE`, `JOURNEY_MODEL`, `JOURNEY_ARTIFACT`
   and `JOURNEY_EVIDENCE` (an empty directory under
   `.archon/reports/journeys/assets/`). It checks that each is set, that
   `HOME` lies under `.archon/journeys/homes/`, and that the engine's `env`
   variables are set. For a `populated` install it then runs the journey's
   `populate` script in that home. Only when every step succeeded does it
   `exec` the journey's `run` command, or `command` when given. On any
   failure it prints `ENV-FAULT: <why>` to stderr, exits 3 and launches
   nothing.
3. **`.archon/journeys/start.sh` and `stop.sh`**, if the journeys need
   private services. `start.sh` records PIDs or Compose projects under
   `.archon/journeys/run/` and waits with a bounded timeout for a health
   check. `stop.sh` stops exactly what `start.sh` recorded, by PID, process
   group or Compose project, never by name or pattern (`pkill`, `killall`,
   `pgrep -f`), and exits 0 once none of them runs.
4. **`.archon/journeys/env.md`** for the next agent: how to run one journey
   (the `run.sh` call), the display or
   headless setup the artifact needs, where logs go, and known quirks.
5. **Prove it works.** For one journey, launch its artifact through
   `run.sh`, confirm that it starts, and stop it. Confirm that none of the
   processes you recorded is left running, and that the installed daemons'
   state was not touched.
6. **Prove it fails closed.** Run `run.sh` with one of the engine's `env`
   variables unset and, as `command`, a sentinel that creates a file.
   Confirm that it exits 3 and the file does not exist. Do the same with a
   `populate` script that fails, when a journey has one.

## Report

Call `complete_task`. The `summary` lists each journey with its artifact
and its runs (journey, install, engine), how to run one, the smoke check and
its result, the files you created, and, on a feedback round, which failures
were environment problems (and fixed) and which were product defects.
