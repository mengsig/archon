# Changelog

## 1.1.0

Released 2026-10-14.

Work needs fewer agents and fewer tokens, and you can change it while it
runs. `archond` now pushes and opens the pull request itself. The
orchestrator answers code questions from its own copy of your repositories
and delegates research to helpers. `archon queue steer` changes a running
item without restarting it. Dependent changes stack, a change across
repositories is one initiative, and `archond` learns which models pass for
each stage.

### Upgrading

The store migration is one-way: once 1.1.0's `archond` has started, a 1.0
`archond` refuses the store. Back it up first:

1. `archon daemon stop`.
2. Copy `archon.db` from the data directory (`~/.local/share/archon` by
   default) to a name of your own.
3. `archon update`, then run what it prints: `archon aether mcp refresh
   archon` and `archon orchestrator restart`. It also says when your Aether
   is too old (`aether update`).
4. Shared mode only: add `send_caller_meta = true` to the `archon` MCP
   server in your Aether config and restart your `aetherd` before the
   orchestrator restart. `archon doctor` checks it. With Archon's own Aether
   instance, the first `archon` command adds the line and restarts it once.

To go back to 1.0.1, see "Back up before a release that migrates the store"
in [docs/install.md](docs/install.md): stop `archond`, install 1.0.1 and put
the copy back. What 1.1.0 recorded since is lost. A running orchestrator
keeps the prompt and tools it started with until `archon orchestrator
restart`: until then it cannot delegate, steer, stack or use `adaptive`,
`direct` or its mirrors.

### Breaking changes

- `archon` and `archond` speak protocol 7 (1.0.1 spoke 5), and a client and
  a daemon of different protocols refuse each other ("upgrade the older
  one"): restart `archond` after updating.
- The built-in pipelines have no `ship` stage. A model plan, `--goto` or
  `--remove` naming `ship` for one of them is refused. Items queued or
  retried from 1.0.1 drop their `ship` model plan entry by themselves. A
  pipeline of your own that ends in a `kind = "ship"` stage keeps working.
- `[limits] max_parallel_agents` defaults to 8, up from 3. A config that
  sets it keeps its value.
- `archond` needs git 2.38 or later for the git it runs itself; `archon
  doctor` checks it.
- A cancelled item no longer wakes the orchestrator: it reads the
  cancellation at its next turn. Failures, blockers, successes, items
  tagged `notify` and GitHub requests still wake it, as in 1.0.1. The new
  `[orchestrator] wake_on_success = false` also leaves routine successes for
  its next turn.

### Shipping and review

- **`archond` ships; no agent does.** One stage, usually the last that
  commits, returns a ship record (`commit`, `tree`, `checks`, the pull
  request's `title` and `body`). Once every stage has passed, `archond`
  checks the worktree (the run's own branch, clean, exactly the recorded
  tree, every new commit under the repository's git identity), pushes
  without forcing and without hooks, and opens or reuses the pull request
  with `gh`. That saves about 40 seconds and 166,000 input tokens per item.
  `local` mode runs the same checks and pushes nothing, and `merge = auto`
  asks GitHub to merge once checks allow. An item succeeds once its pull
  request is open: `archond` does not wait for CI or repair a failing check.
- **No program a repository or an agent configures runs as `archond`.**
  Delivery, restacking and worktree removal read files only through
  `archond`'s own git directory under `<data>/daemon-git`, over the
  repository's objects, with no hook, filter, merge driver, fsmonitor,
  `core.sshCommand` or credential helper from the repository or a
  `.gitattributes`. Push and fetch use your global git config and
  `GIT_SSH_COMMAND`, and every git command has a time limit.
- The push sends the commit the checks proved
  (`<commit>:refs/heads/<branch>`), not whatever the branch holds by then,
  and a stacked run missing its parent's commit is refused before anything
  leaves. The git identity commits must carry is recorded at `archon repo
  add`, and again by `archon repo set <repo>`, which with no option
  records only that; a repository added before 1.1.0 has it recorded at
  its first delivery. A `user.email` an agent sets later does not count.
- A refused delivery fails the item as `stage ship: <what to change>`.
  What only you can fix (no `origin`, a lapsed `gh` login, a branch rule)
  is a blocker. Network and GitHub failures are retried five times, and the
  retries survive a daemon restart. Delivery survives a restart and a retry
  without opening a second pull request.
- Auto-merge is owed until GitHub takes it: a failure is retried for about
  two hours, and a refusal is one blocker saying the pull request is
  delivered but will not merge by itself. The feed says "auto-merge is on",
  never that a delivered pull request merged.
- Your own pipeline opts in with `[pipeline.ship] stage = "<id>"`;
  `docs/examples/pipelines/bugfix-tdd` shows how.
- **New built-in `direct`**: one agent implements a small, low-risk change,
  runs the checks it affects and commits. It is the cheapest pipeline that
  ships. There are 18 built-ins.
- **New built-in `adaptive`**: one lead sizes the work, delegates the parts
  worth splitting, integrates them, and has the integrated diff reviewed by
  fresh reviewers at the item's review level.
- **The review level belongs to the item.** `none`, `standard` or `strict`
  is no, one or three independent reviewers in `adaptive` and `direct`,
  within your autonomy ceiling. A pipeline whose review stages are its own
  refuses another level.
- **A review pass is checked, not believed.** The lead names each
  reviewer's agent id, and each reviewer returns a structured verdict on
  the commit it read. Archon accepts only completed direct children, and
  every commit it delivers must have been read by enough passing
  reviewers. When the reviewed stage also writes the ship record, the
  commit they passed must hold the tree that ships. A failed verdict
  stands until a later pass reads past it, and a fix can be reviewed as a
  delta.
- **A model plan is optional.** A stage left out gets the learned choice,
  else the orchestrator's model where your policy allows it, else the first
  allowed model. The item shows the filled plan.
- **A stage can hand a task back** with `ESCALATE:` (too large or risky for
  its pipeline, with a recommendation) or `BLOCKED:` (what it needs from
  you). Nothing ships, and the orchestrator moves the item to a pipeline
  that fits.
- Reviewers and verifiers rely on a full-suite result only when it names
  the tree they review (`git rev-parse HEAD^{tree}`), and otherwise run the
  suite themselves. The `test-coverage` verifier repeats timing-sensitive
  tests and fails a high-risk test without sensitivity evidence.
- The contracts of all built-in stages together are 15 % shorter, with
  every safety rule kept.

### Orchestrator

- It answers, delegates or queues, taking the cheapest that fits. A
  question about your code needs no item: it reads a **mirror**, a local
  clone of each registered repository at its base commit, with no remote,
  no hooks and a hook that refuses any push. Its git directory is under
  `<data>/mirror-git`, out of a helper's reach, so nothing a helper writes
  runs as `archond`. A finished item refreshes its own repository's mirror,
  and your next message refreshes them all. A refresh that fails is said
  once, and holds nothing up.
- Wide reads, research and second opinions go to **helpers** through
  Aether's `delegate`. A helper works in a mirror or a scratch directory.
  It is cancelled if it lands in a registered repository or a worktree,
  or runs a model `[models]` refuses. Helpers count toward budgets, not caps.
  A helper may delegate once more. Every change to a repository is still an
  item, under the pipeline's gates, the caps and the autonomy ceiling.
- It is woken for a failure, a blocker, a success, an item tagged
  `notify`, a GitHub request and a finished initiative. Other news comes
  with its next turn. Urgent news (your answer, a blocker, an opened gate,
  report feedback, an escalation, a budget hold) is delivered at once
  instead of waiting out the coalescing window.
- Its prompt is 12 KiB, down from 19, and its tool descriptions are 18.0 kB,
  down from 30.1: about 5,000 fewer tokens in every context. Directives are
  sent in full once, then only as changes.
- Only the current orchestrator may call Archon tools that change
  something. A replaced orchestrator, a helper or a stage agent is refused.
  The helpers of a replaced orchestrator are cancelled.
- A stage agent's `message_orchestrator` is written to the feed whole, and
  an escalation shows in `archon chat` as a blocker.

### Steering and the fix loop

- `archon queue steer <id>` and `steer_item` change an item without
  cancelling it. `--note` amends the brief for every agent working on it;
  `--model`, `--effort`, `--budget-usd` and `--extend` change the running
  stage; `--abort`, `--remove`, `--insert` and `--switch` change the stages
  still to come. The model policy, the budgets and the required review hold
  as for an enqueue. While a run delivers, only a note is taken, and no
  steer may leave a run with nothing to ship. A steered run survives a
  daemon restart.
- A failed item given `--insert` or `--switch` is queued again on its own
  branch, with what it committed. `archon queue show`, `queue list`,
  `status` and `get_item` name the pipeline a steer switched an item to.
- Every steer is kept with the item: who made it, when and what it
  changed, including a review level it raised or lowered. A steer is not a
  verdict on the work: learning does not count it against a model.
- The implementer is kept through its fix loop: it waits while the review
  runs and then gets the findings, with its context intact, and the fix
  attempt's own budget on top of what it already spent. A fix whose final
  message leaves out the ship record is asked for it once before the stage
  fails. Reviewers stay fresh. A stage's prompt carries `## Handoff from
  earlier stages`.

### Stacked changes and initiatives

- An item that `depends_on` an unmerged item of its repository is stacked
  on it: its branch forks from the parent's, its pull request targets the
  parent's branch, and its reviewer sees the diff from that base. Once a
  parent merges, `archond` retargets the child and merges forward, never
  force-pushing. A conflict queues a fix item, which never runs beside its
  item. Restacking merges in `archond`'s own git directory and writes
  nothing on a conflict. Only a dependency that shipped is stacked on.
- GitHub is asked once per repository per maintenance pass, and a merged or
  closed pull request is not asked about again. A stack or initiative whose
  maintenance fails is its own blocker, told once, and no longer stops the
  others.
- A change across repositories is one **initiative**: `enqueue_initiative`
  creates all its members or none, with a shared contract of up to 2 KiB,
  and `update_initiative` appends to the contract or replaces it; running
  members get the whole new text. An atomic initiative opens draft pull
  requests and readies them together once every member passed. It never
  merges them: you merge the set. A failed member fails the initiative and
  holds its queued members.
- `archon initiatives` lists them, `create <file>` starts one from a JSON or
  TOML file, and `update <id> --append-file` or `--contract-file` extends
  or replaces its contract. `show`, `update`, `cancel` and `retry` take an
  `ini_…` id or its start, as item commands do.

### Learning

- `archond` records every finished run's outcome, cost, wall time, diff
  size and corrections: a retry, a rejected gate, a correction you send as
  `correction {"item":"qi_…","stage":"…","cause":"quality","reason":"…"}`
  and, for 14 days, a pull request review that requests changes or a close
  without merge. Only a `quality` correction counts against the stage it
  names: steers, scope, effort, infrastructure and budget do not. A stage
  whose review Archon refused counts as no pass.
- Learning never holds up a run. It happens after the run finished; a gap
  is filled by a recurring reconciliation, and a run whose record cannot be
  read is skipped with a marker. Costs come from the spend ledger, so they
  include what the provider settles after the run ends, and a stage steered
  to another model is recorded per model.
- A stage left out of a model plan gets the cheapest allowed choice whose
  pass rate is, with confidence, close to the best's, from runs of the same
  repository, pipeline revision, review level, task class (`task:<class>`)
  and diff size (`diff:small`, `medium` or `large`). Up to 10 % of items try
  another ready, allowed model, one with no history included; one clearly
  worse over five runs is not tried. A plan you wrote is never changed.
- `archon lessons` records, lists, edits, pins, retracts and accepts short
  lessons. A run's agents get at most six, quoted as untrusted advice. A
  lesson the orchestrator records names its source item, stays in that
  item's repository, and is proposed, with low weight, until a clean run
  confirms it or you accept it. Confidence decays from a lesson's last use,
  so age alone never retracts one that wins. Read-only `lessons` commands
  work with the daemon stopped.
- Every 20 outcomes of a repository queue a $1 errand that proposes lessons
  and pipelines: one per repository at a time, and at most $3 a day in all
  (`distill_daily_budget_usd`). History from before the update informs
  routing but is never distilled. `archon pipelines proposals` shows a
  proposed pipeline before `archon pipelines accept`.
- `[learning]` holds `enabled`, `distill_every`, `distill_daily_budget_usd`
  and `explore_rate`.

### Pipelines

- `allow_delegation = true` lets a stage agent delegate under `tools =
  guarded`. A delegating agent gets a `## Delegation` section: leaf children
  only, isolated workspaces for editing, the model policy, and the rules
  each child must be given. When it ends, Archon stops what it left running
  before the next stage starts.
- No stage agent sees Aether's schedule tools.
- `[pipeline.review]` declares the review a work stage holds.

### Speed and cost

- Independent runs start, spawn and ship in parallel, up to 8 at a time.
  A slow Aether call or `git push` holds back only its own run or
  repository.
- A usage update runs no dispatch pass unless a budget is set, and an early
  stage event is settled when its spawn is recorded.
- A stage's cost includes the agents it delegated to, shown per agent in
  `archon queue show`. A turn is charged what the provider billed instead
  of the higher of that and the list price.

### Setup

- `archon setup` writes a `config.toml` with every key commented out when
  there is none.
- `archon doctor` warns about a pre-1.0 `archon.toml`, which is still not
  read.
- `archon daemon stop --aether` waits until Archon's `aetherd` is gone.
- A detached start appends to the daemon log beside the socket
  (`archond.log`) instead of truncating it, after a line marking the start.
  Past 16 MiB the log moves to `archond.log.1` first. A failed autostart
  shows only that start's part.
- Under `--json`, a command line that does not parse prints the JSON error
  envelope (`invalid_argument`, exit 2) instead of clap's text.
- Limits that were fixed are now `[limits]` keys with the old values as
  defaults: `max_delegation_depth` (2), `max_agent_messages` (20),
  `initiative_members` (16) and `initiative_contract_bytes` (2048). Each
  refusal names its key.

## 1.0.1

Gates and questions can now be answered in the orchestrator's chat, so
Archon is usable from a surface that is only a chat.

### Answering from chat

- `/answer <n> <answer>`, sent to the orchestrator as the whole message,
  answers a gate or a question. `archond` applies it itself, through the
  same checks and with the same feed record as `archon answer`, marked
  `answered from operator chat`. For a proposal or report gate the answer
  is `approve`, `reject`, or any other text, which is feedback that sends
  the work back.
- When a proposal or report gate opens, the orchestrator restates it in
  chat: what is asked, a short summary, its number and the `/answer` line
  that answers it.
- A plain reply to a question the orchestrator asked itself is now recorded
  by the orchestrator, so the question leaves `archon questions`.
- This works in the terminal UI, and is what Archon provides for the remote
  app of an Aether that has one. See
  [answering in the orchestrator's chat](https://github.com/mengsig/archon/blob/main/docs/guide.md#answering-in-the-orchestrators-chat)
  and
  [using Archon from Aether's remote app](https://github.com/mengsig/archon/blob/main/docs/guide.md#using-archon-from-aethers-remote-app).

After updating: run the commands `archon update` prints. The last one,
`archon orchestrator restart`, gives the orchestrator its new instructions;
`archond` applies `/answer` without it. Then run `archon setup --resync`,
which lists the orchestrator's new `record_chat_answer` tool in Archon's
Aether registration. In shared mode, add `record_chat_answer = "mutation"`
to the `classes` table of the `archon` MCP server in your Aether config
instead.

Archon needs the same Aether as before: one that serves a protocol from 14
to 17.

### Known limitations

- Chat answers are off with `[aether] mode = "shared"`: there, a client
  that connects with the Aether daemon's `[listen]` token is the same
  `user` actor as you. `/answer` is refused with that reason; use
  `archon answer`.
- Only your own message counts, and only when `/answer` is all of it. The
  orchestrator cannot answer a gate, and it records a plain reply only for
  a question it asked itself.
- A command is applied at most once. If `archond` stops in the instant
  between reading it and applying it, nothing is answered; the question
  stays in `archon questions`, so send it again.
- The remote app has not been tested with Archon. A gate's Lavish page does
  not open on a phone, there are no push notifications for gates, and a
  device paired as an observer cannot answer.

## 1.0.0

First public release.

Archon runs coding tasks through a queue and review pipelines, with agents
on the [Aether](https://github.com/mengsig/aether) daemon.

### Queue and pipelines

- A persistent queue of work items across repositories, with priorities,
  dependencies, tags, retry and cancel. You describe work to the orchestrator
  (`archon`, `archon say`), or script it with `archon queue add` and
  `archon queue wait`.
- Sixteen built-in pipelines, among them `quick-pr`, `default-pr-review`,
  `strict-pr-review`, `bugfix`, `docs`, `team-pr`, `code-audit` and
  `design-doc`.
- Your own pipelines: a `pipeline.toml` and one Markdown prompt per stage,
  for every repository or for one. `archon pipelines check` validates them,
  and a pipeline with a built-in's name replaces it.
- Review and test gates with fix loops: a failed gate sends the work back a
  bounded number of times. A stage can fan out to independent reviewers, or
  to team roles that work in parallel.

### Worktrees and shipping

- Every item on a repository works in its own git worktree and branch, so
  parallel items and your checkout do not disturb each other.
- Two ship modes, set per repository: `pr` opens a pull request and `local`
  keeps the commits on the run branch.
- The build caches and worktrees of finished runs are collected
  automatically, and by `archon gc` on request. Nothing new starts while
  free disk space is low.

### Limits and control

- Caps on running agents, overall, per repository and per pipeline, and
  budgets per day, in total and per item.
- A model policy: the models that may run and the highest reasoning effort.
- An autonomy ceiling over five knobs per item: review depth, proposal
  first, report before the pull request, automatic merge and tool access.
  `archond` refuses an item above it, and only you raise it. Merging is
  manual by default.
- Operator gates: an item can wait for your approval of a proposal before
  implementation starts, or of a change report before the pull request.
- `archon status`, `cost`, `log`, `questions`, `answer`, `pause` and
  `resume`, and `--json` output for scripts.

### GitHub watcher

- `archon github-watcher` routes mentions of Archon on the issues and pull
  requests of watched repositories to the orchestrator, which answers
  there. Only people with write access or more can ask, an allowlist can
  narrow that, and only the logins you list as operators speak with your
  authority.

### Reports and interfaces

- Lavish reports: proposals and change reports are HTML pages, shown as
  annotated review pages when `lavish-axi` is installed. Without it the
  files and `archon answer` do the same job.
- `archon` opens the terminal UI: Aether's TUI on the orchestrator, in a calm
  view that keeps replies and decisions and hides tool detail. `archon tui`
  is the same with options.
- `archon chat` is a quiet line chat with the orchestrator, for SSH, scripts
  and pipes. A bare `archon` opens it instead, and says why, when it is not
  on a terminal or `[aether] address` is set.

### Install and update

- `install.sh` installs a release binary for Linux (x86_64 and aarch64,
  statically linked) or macOS (Apple silicon and Intel), checks its SHA-256,
  and offers to install Aether.
- `archon update` replaces the installed binary with a newer release;
  `--check` only reports, and `--version` installs one release exactly.
- `archon setup` prepares Archon's own Aether instance, and `archon doctor`
  checks the installation.

### Known limitations

- Archon is not a sandbox. Agents run as your Unix user, and an agent's
  shell can run the operator CLI. Read the
  [trust model](https://github.com/mengsig/archon/blob/main/SECURITY.md#trust-model)
  before you widen access.
- Budgets are best-effort: spend is counted as it is reported, so a running
  turn can overshoot.
- The terminal UI shows the orchestrator's transcript, not Archon's feed.
  Notices, questions and item outcomes appear there as the orchestrator
  restates them, and an operator gate is answered in Lavish or with
  `archon answer`, not in the terminal UI. See
  [what the terminal UI shows](https://github.com/mengsig/archon/blob/main/docs/guide.md#what-the-terminal-ui-shows-and-what-it-does-not).
- Linux and macOS only, and macOS is not covered by CI.
