# Changelog

## Unreleased

The orchestrator answers, reads your code and delegates by itself, is
woken only for what needs it, and a pipeline stage can delegate again.

### Orchestrator

- The orchestrator decides how a request is done, taking the first of these
  that fits: answer it, delegate it, or queue it through the cheapest
  pipeline that fits. A question about your code no longer takes an item:
  the orchestrator reads the code and answers in the same turn. `ask` is
  still a pipeline, and nothing requires it any more.
- It reads code in mirrors. For every registered repository `archond`
  keeps `<data>/orchestrator/repos/<name>`: a local clone checked out at
  the base branch, with no remote and none of the repository's branches or
  hooks, and with a hook that refuses any push. It is made when the
  repository is registered, refreshed when an item finishes and when you
  write, and removed when the repository is unregistered. It shares the
  repository's objects through hard links, so it costs about one more
  checkout. A refresh that fails is said once in the feed and to the
  orchestrator, and holds nothing up. A repository whose own `[models]`
  refuses the orchestrator's model gets no mirror.
- Work that would flood the orchestrator's context or splits into parts
  (research, a wide read of a subsystem, a comparison, a second look) goes
  to a helper agent through Aether's `delegate`. A helper works in one
  mirror, shared to read it or as an isolated copy to build in, or in a
  scratch directory. `archond` cancels one that Aether placed in a
  registered repository or among the items' worktrees, also through a
  symlink, with a blocker and a `policy_violation` for the orchestrator.
- A helper must use a model and effort that `[models]` allows and that the
  repository whose mirror it reads does not refuse; one outside is
  cancelled. Other repositories' `[models]` no longer narrow it. Helpers
  count toward the daily and total budgets. `max_parallel_agents` and the
  other caps do not count them: the orchestrator is told to stay within
  them.
- A helper may delegate once more, and so may a child of a delegating stage
  agent. `archond` cancels an agent a third delegation down.
- The orchestrator is told never to delegate for a GitHub request without
  operator standing. Only its prompt holds this.
- A helper gets only the instructions the orchestrator writes for it; none
  of the orchestrator's own rules are copied. Its prompt now has it state
  them for every helper: commit and push nothing, stay in the workspace
  given, no git identity overrides, no questions to the operator.
- A finished item no longer wakes the orchestrator unless it has something
  to do about it. `archond` posts every outcome to your feed itself, as
  before. The orchestrator is woken for a failure, a blocker, an item
  tagged `notify` and an item a GitHub thread asked for, and reads the
  other outcomes as one short list at its next turn; your next message
  delivers that list first. One wake is a whole turn over the
  orchestrator's context, and almost all of them were routine successes.
  `[orchestrator] wake_on_success = true` restores a wake per outcome.
  In `archon chat` nothing changes. The terminal UI shows only what the
  orchestrator says, so there a routine success now appears with its next
  turn, not as it happens.
- A message a stage agent sends with `message_orchestrator` is written to
  the feed by `archond`, whole: an escalation shows in `archon chat` as a
  blocker. The orchestrator acts on it and no longer repeats it.
- Operator directives are sent to the orchestrator in full once, and after
  that only as what was set and cleared, with a count of how many are in
  force. They are sent in full again after an orchestrator restart, a
  revival and a compaction of its context. They were most of every trigger.
- The orchestrator's prompt is 12 KiB, down from 19, and the descriptions
  and schemas of its 25 Archon tools 17.7 kB, down from 30.1: roughly 5,000
  fewer tokens in every orchestrator context. What a tool does is said in
  the tool's description only.
- `[limits] max_parallel_agents` defaults to 8, from 3. A config that sets
  it keeps its value.
- `archond` does less while agents work: a usage update no longer runs a
  dispatch pass unless a total or daily budget is set, and a stage whose
  first events arrive before its spawn is recorded is settled when the
  record is written, not at the next 100 ms poll.
- An answer the orchestrator finishes after waiting for a helper is no
  longer cut off at the wait. `archon chat` shows two lines for it: what the
  orchestrator said before it waited (Aether has it say something first),
  and the answer once the helper is done.
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
- For a change the orchestrator takes the cheapest pipeline that fits and
  says what it expects the choice to save: `quick-pr` for a small, low-risk
  change with an obvious regression test and no need of review,
  `default-pr-review` for bounded work by one author that needs one
  independent review, `adaptive` where one lead that keeps its context
  through fixes or integrates independent parts saves restarts. It never
  bundles unrelated changes to use `adaptive`. It plans `x_high` or `max`
  only for a reasoning problem it can name that inspecting the work left
  unresolved: that you prefer strong models is not one.
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
  Archon cancels whatever it left running and starts the next stage or the
  replacement only once Aether holds none of those agents as running:
  Aether stops agents cooperatively, so its answer to the cancel is not
  taken for the stop. The stop is stored first, so it survives a daemon
  restart, and it stays owed while Aether is unreachable. Agents still
  running two minutes after the cancel are named in a blocker; the item
  waits for them and goes on by itself.
- A pipeline can declare the review one of its stages holds:
  `[pipeline.review] level = "standard"`, `stage = "<a delegating work
  stage>"`. It then runs at `review = standard`, and Archon fails the run
  at that stage unless its agent completes with the review on record: a
  pass naming the reviewer's model, or a skip with its reason.
- A recorded pass is checked, not believed. As the stage ends Archon asks
  Aether which agents its lead delegated to, and the pass counts only when
  one of them completed on the model it names; a cancelled or failed
  reviewer does not count. The run fails at that stage otherwise, saying
  what the lead did delegate to. The reviewer's agent id is stored with the
  evidence and shown in the `REVIEW:` line. This proves a reviewer on that
  model ran to completion, not what it concluded. A skipped review is shown
  with its reason in `archon queue show` and `get_item`.
- New built-in pipeline `adaptive` (`build` → `ship`), for a change where
  one lead that keeps its context through fixes, or integrates independent
  parts, saves restarts. One lead sizes the work, delegates the parts worth
  splitting, integrates and verifies them, and has the integrated diff
  reviewed once by a fresh reviewer, at a depth matching the risk. It
  declares that review, so it runs at `review = standard`: name it with
  `pipeline = "adaptive"` and leave `review` out or say `standard`.
  `review = strict` or `none` with it is refused, and so is a ceiling that
  demands `strict`. `ship` ships only a build whose review passed or was
  skipped with a reason. There are now 17 built-in pipelines.
- `adaptive` spends less for the same checks. Its lead delegates a worker
  only when the parallel saving exceeds the briefing, the integration and
  the repeated reading; commits before the review; runs the full suite once,
  while the reviewer reads; hands the reviewer the exact commits, changed
  paths, check results and known risks instead of the conversation; and has
  a fix reviewed again, as a delta by a new reviewer, only after a failed
  verdict or when the fix changes production behaviour, security or an
  invariant, or adds scope. Its `build` nudge asks for high effort, medium
  for a bounded familiar change, and `x_high` or `max` only for a named
  reasoning problem. Only an unresolved blocking finding fails the stage;
  the prompt used to say any open finding does.
- A ship stage never amends, rebases, squashes or resets the commits it is
  given. It may reword a commit message that breaks a repository rule, and
  then shows that the tree hash did not change. `adaptive`'s lead is told
  the commit rules (the repository's own, else Conventional Commits with a
  lowercase imperative subject; its configured Git identity; no model or
  tool attribution), and its `ship` commits nothing and fails on a tree
  other than the one the lead verified. A live run had `ship` amend a
  reviewed commit to drop an attribution line the lead had added.
- Every delegating stage agent is now told to hand a child what it already
  holds rather than have it read again, and what a reviewer's hand-off
  contains.
- The agents of an item queued after others (`depends_on`) are told where
  each dependency left its work: its item id, branch and pull request, in
  a `## Dependencies` section of their contract. A dependent item's branch
  still forks from the repository's base, so a change the brief stacks on a
  dependency's branch used to be reviewed as a diff from the base, the
  dependency's change included. The section defines the base of such a
  change, and the reviewer is handed it. Archon still does not stack items
  itself: the branch and the pull request of a dependent item start from
  the base.

### Cost

- A stage's cost now includes the agents it delegated to. `archon queue
  show`, `get_item` and `get_stage` show the split for a stage that
  delegated: its own agent's cost, the delegated agents' cost, and each
  delegated agent with its model, effort and cost (`delegations` and
  `delegation` in the JSON). Run and item totals still count every agent
  once. `model_evidence` compares the complete stage cost, so a lead that
  delegates no longer looks cheaper than it was.
- While a turn runs, Aether prices it at list price, and when the turn ends
  it reports what the provider billed. Archon kept the higher of the two
  and logged a warning on every turn. It now charges the provider's
  complete figure, counts all of it as billed, and takes the difference
  back from that agent's own days only. A total that goes down any other
  way still keeps what was charged, and a provider-reported total never
  lowers a charge below an earlier one, so reporting less cannot evade a
  budget.

### Setup and upgrades

- Archon's tools need Aether to name the agent behind each call:
  `send_caller_meta = true` on the `archon` MCP server. With Archon's own
  Aether instance (the default) nothing is asked of you: the first `archon`
  command after the update adds the line to the instance's config and
  restarts Archon's `aetherd` once, since Aether reads it only at start.
  It says so before the restart, which takes some seconds, and runs once
  `archond` has reconnected, so it no longer reports `aether:
  disconnected` for a restart it made itself. `archond` now looks for an
  `aetherd` that is not running every second instead of at a growing
  interval of up to 30 seconds, so any restart of Aether is picked up at
  once.
  In shared mode Archon never edits your Aether config: `archon doctor`
  fails with the line to add, and your `aetherd` needs a restart after it.
- Until Aether names the callers, `archond` posts one blocker saying so,
  takes those tools only from an orchestrator that cannot delegate, and
  starts new orchestrators without `delegate`. An orchestrator started by
  an earlier release cannot delegate, so it keeps working. One that was
  started with `delegate` before `archond` saw an unnamed call has that
  call refused, with the fix and the `archon orchestrator restart` that
  replaces it. After the fix, the orchestrator's next named call of any
  kind ends this: it used to take a call that changed Archon, so an
  orchestrator that only read for a while kept `archond` starting its
  successors without `delegate`.
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
delegates nor knows `adaptive` or its mirrors until a new one starts, and
it still expects to be woken for every finished item. Until then it is
sent every directive in every trigger, as its prompt expects. In shared
mode, add
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
