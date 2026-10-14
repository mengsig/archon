# Changelog

## Unreleased

`archond` now ships the work itself, stacks dependent changes, coordinates
a change across repositories, lets you steer an item without restarting it,
and learns from what its runs did. The orchestrator answers, reads your code
and delegates by itself, is woken only for what needs it, and a pipeline
stage can delegate again.

### Orchestrator

- The orchestrator decides how a request is done, taking the first of these
  that fits: answer it, delegate it, or queue it through the cheapest
  pipeline that fits. A question about your code no longer takes an item:
  the orchestrator reads the code and answers in the same turn. `ask` is
  still a pipeline, and nothing requires it any more.
- It reads code in mirrors. For every registered repository `archond`
  keeps `<data>/orchestrator/repos/<name>`: a copy checked out at the base
  branch, with no remote and none of the repository's branches or hooks,
  and with a hook that refuses any push. Its git directory is under
  `<data>/mirror-git`, out of a helper's reach, and its config is rewritten
  before every command, so nothing a helper writes runs as `archond`. It is
  made when the repository is registered, refreshed when an item finishes
  and when you write, and removed when the repository is unregistered. It
  costs about one more copy of the repository. A refresh that fails is said
  once in the feed and to the orchestrator, and holds nothing up. A
  repository whose own `[models]` refuses the orchestrator's model gets no
  mirror.
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
- A helper gets only the instructions the orchestrator writes for it; none
  of the orchestrator's own rules are copied. Its prompt now has it state
  them for every helper: commit and push nothing, stay in the workspace
  given, no git identity overrides, no questions to the operator. It is
  told never to delegate for a GitHub request without operator standing;
  only its prompt holds this.
- Every change to a repository is still an item, so the pipeline's gates,
  the ship mode, the caps and the autonomy ceiling apply. The orchestrator
  cannot integrate a helper's edits.
- For a change the orchestrator takes the cheapest pipeline that fits and
  says what it expects the choice to save: `direct` or `quick-pr` for a
  small, low-risk change, `default-pr-review` for bounded work by one author
  that needs one independent review, `adaptive` where one lead that keeps its
  context through fixes or integrates independent parts saves restarts. It
  sets the item's review level by risk, never bundles unrelated changes to
  use `adaptive`, and leaves the model plan out unless you asked for a model
  or it has a reason it writes down. It plans `x_high` or `max` only for a
  reasoning problem it can name that inspecting the work left unresolved:
  that you prefer strong models is not one.
- It changes an item instead of restarting it (`steer_item`), queues a chain
  of dependent changes at once instead of waiting for each merge, and makes
  a change across repositories one initiative (`enqueue_initiative`,
  `update_initiative`). An item that failed with `ESCALATE:` is moved to a
  pipeline that fits, not retried. See the sections below.
- A finished item no longer wakes the orchestrator unless it has something
  to do about it. `archond` posts every outcome to your feed itself, as
  before. The orchestrator is woken for a failure, a blocker, an item
  tagged `notify`, an item a GitHub thread asked for and a finished
  initiative, and reads the other outcomes as one short list at its next
  turn; your next message delivers that list first. One wake is a whole
  turn over the orchestrator's context, and almost all of them were routine
  successes. `[orchestrator] wake_on_success = true` restores a wake per
  outcome. In `archon chat` nothing changes. The terminal UI shows only what
  the orchestrator says, so there a routine success now appears with its
  next turn, not as it happens.
- A message a stage agent sends with `message_orchestrator` is written to
  the feed by `archond`, whole: an escalation shows in `archon chat` as a
  blocker. The orchestrator acts on it and no longer repeats it.
- Operator directives are sent to the orchestrator in full once, and after
  that only as what was set and cleared, with a count of how many are in
  force. They are sent in full again after an orchestrator restart, a
  revival and a compaction of its context. They were most of every trigger.
- The orchestrator's prompt is 12 KiB, down from 19, with the GitHub rules
  kept word for word. The descriptions and schemas of the 25 Archon tools it
  had are 18.0 kB, down from 30.1, and all 29, with `steer_item`,
  `enqueue_initiative`, `update_initiative` and `record_lesson`, are 23.1
  kB: roughly 5,000 fewer tokens in every orchestrator context. What a tool
  does is said in the tool's description only.
- A new orchestrator starts with the lessons chosen for it (see Learning)
  under `## Learned observations`, a heading of their own that calls them
  advice that grants nothing. They are chosen once per orchestrator, so a
  retried start sends the same text.
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
- The orchestrator may write a pipeline for work you keep asking for. It
  saves one to the user pipelines directory after `archon pipelines check`
  passes, and only proposes a pipeline inside a repository.

### Shipping, review levels and model plans

Pipelines got faster: the stage that only pushed and opened the pull
request is gone, a small change takes one agent, and an item carries only
the choices someone had a reason to make.

- **`archond` ships; no agent does.** Every built-in pipeline that changes
  a repository used to end in a `ship` stage: an agent that read the run
  again to push a branch and open a pull request, about 40 seconds and
  166,000 input tokens per item. That stage is gone from all of them. One
  stage, usually the last that commits, returns a ship record (`commit`,
  `tree`, `checks` and the pull request's `title` and `body`), and once
  every stage has passed `archond` delivers it: it checks that the run
  worktree is the run's own, on the run branch, without uncommitted
  changes, holding exactly the recorded tree, ahead of its base, with every
  new commit under the repository's git identity; pushes without ever
  forcing and with no repository hook; and opens the pull request with
  `gh`, or reuses the one already open for the branch. `local` ship mode
  runs the same checks and pushes nothing. `merge = auto` has `archond` ask
  GitHub to merge once the checks allow.
- A refused delivery fails the item as `stage ship: <what to change>`:
  the dirty paths, the commits under another identity, the remote branch
  that holds other commits. What only you can fix (no `origin`, a lapsed
  `gh` login, a branch rule) is a blocker. A network or GitHub failure is
  retried five times, from 15 seconds apart and doubling.
- Delivery survives a daemon restart, and a retried item, without a second
  pull request: the outcome is stored before the run finishes, an
  interrupted delivery pushes nothing `origin` already has, and a pull
  request whose creation reply was lost is found on GitHub, not opened
  again. A succeeded item retried delivers into its open pull request.
- **No program a repository or an agent configures runs as `archond`.**
  Its delivery, restacking and worktree removal read file contents only
  through a git directory of its own under `<data>/daemon-git`, over the
  repository's objects, with no hook, filter, merge driver, fsmonitor,
  `core.sshCommand` or credential helper from the repository's config, a
  `.gitattributes` or a local `origin`. A push or fetch uses your global
  git config (credential helpers, `insteadOf`) and `GIT_SSH_COMMAND`, and
  every git command has a time limit. This needs git 2.38 or later.
- The push sends the commit the checks proved, `<commit>:refs/heads/<branch>`,
  not whatever the branch holds by then. A stacked run missing a parent
  commit is refused before anything is pushed or a pull request opened.
- The identity a ship admits commits under is recorded when you run
  `archon repo add`, and again at `archon repo set`, which says in the feed
  when it changed. A `user.email` an agent sets in the repository later no
  longer counts. A repository registered earlier has it recorded at its
  next delivery.
- Auto-merge is owed until GitHub takes it: a failure is retried for up to
  about two hours, and a refusal is one blocker saying the pull request is
  delivered but will not merge by itself. The feed says "auto-merge is on,
  not merged yet", never that a delivered pull request merged.
- A delivery's retries survive a restart: it neither retries at once nor
  gets five more tries.
- A pipeline of your own opts in with `[pipeline.ship] stage = "<id>"`
  and drops its `ship` stage; `docs/examples/pipelines/bugfix-tdd` shows
  it. The record stage cannot come before a stage that commits. A pipeline
  that still ends in a `kind = "ship"` stage keeps working unchanged.
- **New built-in pipeline `direct`**, the cheapest path to a shipped
  change: one agent implements a small, low-risk change, runs the checks it
  affects, commits and returns the ship record. It has the shortest
  contract and prompt of any pipeline that ships, and no review unless the
  item asks for one. There are now 18 built-in pipelines.
- **The review level belongs to the item, not the pipeline.** `adaptive`
  and `direct` hold their own review, and an item's `review` knob now sets
  how much of it there is: `none`, `standard` or `strict` is no, one or
  three independent reviewers, which the stage agent delegates and Archon
  verifies against Aether one by one. `adaptive` no longer refuses `strict`
  and `none`, and a pipeline may declare any level as its default. Your
  autonomy ceiling still bounds the choice, and raises an item that names
  no level. A pipeline whose review stages are its own runs the level those
  stages give, and an item asking that pipeline for another is refused.
- **A model plan is optional.** `enqueue` and `archon queue add` no longer
  need `model_plan` or `--model-plan-file`, whole or per stage. A stage left
  out gets the choice learned for the repository (see Learning), else the
  orchestrator's model where your policy allows it, else the first allowed
  model, at the effort the pipeline's tier suggests and never above
  `max_effort`. A chosen model is checked against the policy as before, and
  the item shows the filled plan.
- **A stage can hand a task back.** An agent that finds its task too large
  or too risky for its pipeline fails with `ESCALATE:` and what it
  recommends, instead of stretching the pipeline or failing without a
  reason; `BLOCKED:` says what it needs from you. The run ends at once,
  nothing ships, and the orchestrator gets the recommendation with the
  item's failure. `direct` and `quick-pr` use it for work that is not
  small. An escalated item moved to another pipeline (`--switch` or
  `--insert`, below) goes on from its branch.
- Stage contracts are shorter. With the ship stages gone and the repeated
  rules cut, the instructions and delegation sections Archon generates for
  all built-in stages together went from 139,966 to 116,170 bytes (17 %
  less, with `direct` added), with every safety rule kept.

### Steering and the fix loop

- `archon queue steer <id>` and the orchestrator's `steer_item` change an
  item without cancelling it. `--note` amends the brief and is sent to
  every agent working on the item, helpers included. `--model` and
  `--effort` change a stage within the model policy: a running agent ends
  its turn and goes on with its work on the new model, since Aether applies
  a model change at an agent's next turn. `--budget-usd` and `--extend`
  move the budget and the running stage's deadline. Nothing is restarted,
  and later attempts, later stages and retries use what was steered.
- `--abort` stops the running stage and retries it, skips it
  (`--then skip`) or continues at another stage (`--goto`). `--remove`
  takes out a stage that has not started, `--insert` adds one from any
  pipeline, and `--switch` replaces the stages that have not started with
  another pipeline's. A stage that already started keeps its shape, and
  the refusal says so. While a run is delivering its change, only a note
  is taken; and no steer may leave a run without a stage that writes a
  ship record or ships. A stage that hands in its pipeline's ship record
  keeps that role in the run it is carried into when it runs after the
  stage that shipped so far, and an insert that would commit after the ship
  record is refused. The model policy, the budgets and the review the
  autonomy ceiling requires hold for a steer as for an enqueue.
- Steers are recorded as run edits and applied before a run is resumed, so
  a steered run goes on unchanged after a daemon restart. What a steer owes
  the running agents is stored with it and repeated until Aether takes it.
- A failed item given `--insert` or `--switch` is queued again on the same
  branch and worktree with what its failed run committed, so an item whose
  stage gave up continues on a stronger pipeline instead of starting over.
  A switched item keeps its pipeline's name; its steers show what it runs.
- `update_item` no longer refuses `brief_append` and `model_plan` on a
  running item: both are steered into its run. Only an item that is just
  starting its run still refuses them.
- Every steer is kept with the item: who made it, when and what it changed.
  `archon queue show`, `get_item` and the feed show them. Learning counts a
  steer as a correction of the run.
- The implementing agent is kept through its fix loop. Where a gate sends
  work back (`on_fail.goto`) to a `work` stage with one agent, that agent is
  no longer replaced by a cold one for every fix: it waits while the review
  runs and is then sent the findings, with what it learned still in its
  context. Reviewers stay fresh and independent on every attempt. A fresh
  agent takes the fix, with the findings and a digest of the last attempt,
  only when the kept one cannot go on: it is gone, Aether refuses it, or a
  steer changed the stage's model. A kept agent that holds the ship record
  hands in a new one with its fix, and a fix it escalates ends the run.
- A stage's prompt now carries `## Handoff from earlier stages`: what each
  earlier stage, and this stage's own last attempt, reported, at most 1,200
  characters per stage and 4,000 in all, so it need not rediscover it. A
  stage the prompt already shows through `{{previous.<id>.summary}}` is not
  repeated.

### Stacked changes and initiatives

- An item that `depends_on` an unmerged item of its own repository is
  stacked on it: its branch forks from the first unmerged parent's branch,
  further parents are merged in, and `archond` opens its pull request
  against the parent's branch. Its agents are told where each dependency
  left its work (item, branch and pull request) in a `## Dependencies`
  section of their contract, and its reviewer is handed the diff from that
  base, not from the repository's.
- `archond` restacks by itself: once a parent merges, the child's pull
  request is moved to the next unmerged parent or the base branch, and
  commits a parent gains later are merged forward, never rebased or
  force-pushed. A conflict queues a fix item on the child's branch, visible
  in the queue, instead of failing the child.
- Restacking merges in `archond`'s own git directory, writes nothing on a
  conflict, and moves a worktree only when it is archond's and clean. The
  fix item is told the parent commit to `git merge`, and it never runs
  while its item does: the fix waits, and the item is not retried until
  the fix is done. Only a dependency that shipped is stacked on.
- GitHub is asked once per repository per maintenance pass for its open
  pull requests, and a merged or closed one is remembered and never asked
  about again, instead of a `gh pr view` per item and parent every minute.
  Maintenance runs when a run finishes or an initiative changes, and every
  five minutes otherwise.
- A stack or initiative whose maintenance fails is one blocker, told once
  (a GitHub outage after three passes), and no longer stops the others.
- A change across repositories is one initiative: `enqueue_initiative`
  takes a title, a shared contract of up to 2 KiB, and a member per
  repository with its brief and, as in an enqueue, an optional title,
  pipeline, model plan and autonomy, and the earlier members it waits for.
  The members are created together or not at all, an `idempotency_key`
  makes a retried request safe, and `update_initiative` amends the contract
  and tells the members' running agents. Each member's pull request gets a
  "Part of …" section linking its siblings.
- An atomic initiative (`atomic: true`) opens its members' pull requests
  as drafts and readies them together once every member passed. None is
  handed to auto-merge, whatever its repository allows: you merge the set,
  and a member asking for `merge = auto` is refused.
- A member that finishes wakes nobody by itself. One that fails fails the
  initiative, which holds the queued members, and a failed or done
  initiative wakes the orchestrator once (`initiative_finished`). Delivery
  and coordination resume after a daemon restart.
- `archon initiatives` lists them; `show`, `cancel` and `retry` take an
  `ini_…` id. Cancel stops the unfinished members and leaves successful
  branches and pull requests in place; retry resumes failed, cancelled and
  blocked members on their branches, keeping the successful ones.
  `archon queue show` names an item's initiative.

### Learning

- `archond` records an outcome for every finished run: its result, why it
  failed (an `ESCALATE:` and a `BLOCKED:` are told apart from other
  failures), its stage attempts with the agents they delegated to, its cost,
  wall time and diff size, and the corrections it drew. Runs from before the
  update are imported in small batches in the background.
- A correction is a sign that a run that passed was not right: a retry of
  a finished item, a rejected gate, a steer, an operator message naming the
  item and saying it is wrong, and, for 14 days after `archond` delivered a
  pull request, a review requesting changes or a close without a merge, read
  from GitHub every 10 minutes. A merge is recorded too.
- A stage an item leaves out of its model plan gets the cheapest choice
  your policy allows that passed within five points of the best on at least
  five runs of that pipeline's stage, in the repository or else anywhere,
  and of the diff size when the item's `diff:small`, `diff:medium` or
  `diff:large` tag says it. A share of items, 10 % by default and never
  more, tries the next cheaper choice, decided by the item's id. A plan you wrote is never changed, and no model
  call is made to choose.
- `archon lessons` records, lists, edits, pins, retracts and accepts short
  lessons; `archon lessons why <item>` shows the lessons and the model
  evidence an item was given, and `archon lessons stats` each pipeline's
  pass and correction rates, costs and wall times. A run's agents get at
  most six lessons for its repository and pipeline, quoted as untrusted
  advice. A lesson whose uses keep ending in corrections is retracted.
- When an item fails, the orchestrator is woken with a hint to
  `record_lesson` what the failure teaches.
- Every 20 new outcomes of a repository queue one low-priority `errand`,
  limited to $1, that distils lessons and may propose a pipeline. A proposed
  lesson waits for `archon lessons accept`; a proposed pipeline is checked
  like any of yours, `archond`'s delivery rules included, kept as
  `<name>.proposed/` outside the catalog, and used once you run
  `archon pipelines accept <name>`.
- `[learning]` holds `enabled`, `distill_every` and `explore_rate` (0 to
  0.1). `enabled = false` stops recording, learned defaults, lessons and
  distillation; what was recorded stays readable.

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
  `[pipeline.review] level = "standard"`, `stage = "<a work stage>"`.
  Archon fails the run at that stage unless its agent completes with the
  review on record: a pass naming each reviewer's model. How many reviewers
  that is follows the item's review level (see "Shipping, review levels and
  model plans" above).
- A recorded pass is checked, not believed. As the stage ends Archon asks
  Aether which agents its lead delegated to, and the pass counts only when
  one of them completed on each model it names; a cancelled or failed
  reviewer does not count. The run fails at that stage otherwise, saying
  what the lead did delegate to. The reviewers' agent ids are stored with
  the evidence and shown in the `REVIEW:` line. This proves reviewers on
  those models ran to completion, not what they concluded.
- New built-in pipeline `adaptive` (one stage, `build`), for a change where
  one lead that keeps its context through fixes, or integrates independent
  parts, saves restarts. One lead sizes the work, delegates the parts worth
  splitting, integrates and verifies them, and has the integrated diff
  reviewed by fresh reviewers, at a depth matching the risk. Name it with
  `pipeline = "adaptive"`; it runs at `review = standard`, one reviewer,
  unless the item asks for another level. Archon ships only a build whose
  reviewers it verified.
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
  tool attribution), and nothing is committed after it: a tree other than
  the one the lead verified does not ship. A live run had `ship` amend a
  reviewed commit to drop an attribution line the lead had added.
- Every delegating stage agent is now told to hand a child what it already
  holds rather than have it read again, and what a reviewer's hand-off
  contains.

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
  once. In shared mode Archon never edits your Aether config: `archon
  doctor` fails with the line to add, and your `aetherd` needs a restart
  after it.
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
- `archon` and `archond` speak protocol 7: restart `archond` after
  updating, as `archon update` says. The store is migrated when `archond`
  starts.
- `archon setup` writes a `config.toml` with every key commented out when
  there is none, `[learning]` and `wake_on_success` included, and never
  touches one that exists.
- An `archon.toml` from before 1.0 is still not read. `archon doctor` now
  warns about it and `archond` posts one notice as it starts, both naming
  the `config.toml` that is read.
- The documentation says plainly that no models or tiers are predefined:
  without `[models]` every model Aether reports ready may be used, and the
  orchestrator picks one per stage. Examples name current models.

After updating: run the commands `archon update` prints. The last one,
`archon orchestrator restart`, matters here: a running orchestrator keeps
the prompt, tools and workspace it was created with, so it neither
delegates, steers, stacks nor knows `adaptive`, `direct` or its mirrors
until a new one starts, and it still expects to be woken for every finished
item. Until then it is sent every directive in every trigger, as its prompt
expects. In shared mode, add `send_caller_meta = true` to the `archon`
server in your Aether config and restart your `aetherd` before that
restart; `archon doctor` checks it. A pipeline of your own that ends in a
`kind = "ship"` stage needs no change.

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
