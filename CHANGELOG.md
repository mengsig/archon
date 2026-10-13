# Changelog

## Unreleased

The orchestrator answers and delegates work that needs no repository
itself, and a pipeline stage can delegate again.

### Orchestrator

- The orchestrator decides how a request is done, taking the first of these
  that fits: answer it, delegate it, queue it through a pipeline, or queue a
  backlog. Work that needs no repository (research, a comparison, a summary
  or draft, a second look at a result it holds) goes to a helper agent
  through Aether's `delegate`. A question that takes reading a repository's
  code is queued as `ask`, which reads a snapshot, not your checkout.
- A helper holds no repository. It works in the orchestrator's own
  workspace, and `archond` cancels one that Aether placed in a registered
  repository or among the items' worktrees, also through a symlink, with a
  blocker and a `policy_violation` for the orchestrator.
- A helper must use a model and effort that `[models]` allows and that no
  registered repository's own `[models]` refuses; one outside is cancelled.
  Helpers count toward the daily and total budgets. `max_parallel_agents`
  and the other caps do not count them: the orchestrator is told to stay
  within them.
- A GitHub request without operator standing is never delegated.
- A helper gets only the instructions the orchestrator writes for it; none
  of the orchestrator's own rules are copied. Its prompt now has it state
  them for every helper: read only, stay in the workspace given, open no
  repository, no git identity overrides, no questions to the operator.
- An answer the orchestrator finishes after waiting for a helper is shown
  in `archon chat` as one reply, not cut off at the wait.
- Only the current orchestrator may call the Archon tools that change
  something. `archond` decides this itself for every call, from the calling
  agent that Aether names. A replaced orchestrator, a helper and a stage
  agent are refused, and the refusal says which of them the caller is.
- When the orchestrator is replaced, the helpers of every agent left
  behind are cancelled, also when Aether restarted it several times in a
  row. A cancel that fails is kept on record and retried in the background
  until Aether confirms it, across daemon restarts.
- Every change to a repository is still an item, so the pipeline's gates,
  the ship mode, the caps and the autonomy ceiling apply. The orchestrator
  cannot integrate a helper's edits.
- The orchestrator may write a pipeline for work you keep asking for. It
  saves one to the user pipelines directory after `archon pipelines check`
  passes, and only proposes a pipeline inside a repository.

### Pipelines

- `allow_delegation = true` on a stage grants delegation again: its agent
  may delegate under `tools = guarded` too. Without the flag a guarded stage
  agent stays a leaf, and `tools = yolo` still lets every stage agent
  delegate. Which Archon tools a stage agent sees still follows `tools`.
  Delegated children work in Aether's managed Git worktrees, which share
  the repository's Git objects and leave out ignored caches; what a child
  builds still costs disk.
- A delegated child inherits none of its parent's instructions. Every stage
  agent that can delegate, by the flag or by `tools = yolo`, now gets a
  `## Delegation` section in its contract: leaf children only, an isolated
  workspace for a child that edits, the model policy its children are held
  to, to wait for every child, and the rules to state in each child's
  instructions (stay in the given workspace, keep off the operator's
  checkout, no push, no git identity overrides, no operator questions, read
  only for reviewers). A pipeline of your own that delegates should name
  any further constraint its children must be given.
- No stage agent sees Aether's `create_schedule`, `update_schedule` or
  `list_schedules`: a schedule would outlive the stage. In a `repo-readonly`
  workspace `integrate_workspace` is hidden as well.
- When a stage agent that could delegate completes, fails or is replaced,
  Archon cancels whatever it left running before the next stage or the
  replacement starts. The stop is stored first, so it survives a daemon
  restart, and it is retried while Aether is unreachable.
- A pipeline can declare the review one of its stages holds:
  `[pipeline.review] level = "standard"`, `stage = "<a delegating work
  stage>"`. It then runs at `review = standard`, and Archon fails the run
  at that stage unless its agent completes with the review on record: a
  pass naming the reviewer's model, or a skip with its reason.
- New built-in pipeline `adaptive` (`build` → `ship`), for ordinary
  repository changes. One lead sizes the work, delegates the parts worth
  splitting, integrates and verifies them, and has the integrated diff
  reviewed once by a fresh reviewer, at a depth matching the risk. It
  declares that review, so it runs at `review = standard`: name it with
  `pipeline = "adaptive"` and leave `review` out or say `standard`.
  `review = strict` or `none` with it is refused, and so is a ceiling that
  demands `strict`. `ship` ships only a build whose review passed or was
  skipped with a reason. There are now 17 built-in pipelines.

### Setup and upgrades

- Archon's tools need Aether to name the agent behind each call:
  `send_caller_meta = true` on the `archon` MCP server. With Archon's own
  Aether instance (the default) nothing is asked of you: the first `archon`
  command after the update adds the line to the instance's config and
  restarts Archon's `aetherd` once, since Aether reads it only at start.
  In shared mode Archon never edits your Aether config: `archon doctor`
  fails with the line to add, and your `aetherd` needs a restart after it.
- Until Aether names the callers, `archond` posts one blocker saying so,
  takes those tools only from an orchestrator that cannot delegate, and
  starts new orchestrators without `delegate`. An orchestrator started by
  an earlier release cannot delegate, so it keeps working. One that was
  started with `delegate` before `archond` saw an unnamed call has that
  call refused, with the fix and the `archon orchestrator restart` that
  replaces it.
- `archon daemon stop --aether` now waits until Archon's `aetherd` is gone,
  and a start that still finds the old one holding its database retries
  for up to 15 seconds instead of failing with "another aether daemon is
  already running".
- `archon` and `archond` speak protocol 6: restart `archond` after
  updating, as `archon update` says.
- `archon setup` writes a `config.toml` with every key commented out when
  there is none, and never touches one that exists.
- An `archon.toml` from before 1.0 is still not read. `archon doctor` now
  warns about it and `archond` posts one notice as it starts, both naming
  the `config.toml` that is read.
- The documentation says plainly that no models or tiers are predefined:
  without `[models]` every model Aether reports ready may be used, and the
  orchestrator picks one per stage. Examples name current models.

After updating: run the commands `archon update` prints. The last one,
`archon orchestrator restart`, matters here: a running orchestrator keeps
the prompt, tools and workspace it was created with, so it neither
delegates nor knows `adaptive` until a new one starts. In shared mode, add
`send_caller_meta = true` to the `archon` server in your Aether config and
restart your `aetherd` before that restart; `archon doctor` checks it.

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
