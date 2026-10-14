# Writing Archon pipelines

A **pipeline** is a directory: one `pipeline.toml` plus one Markdown prompt
per stage. To add a pipeline, you drop a directory in place. You do not
change any code, rebuild anything, or restart the daemon. Archon reads the
catalog again at every enqueue, so a new or edited pipeline is live for the
next item.

This guide covers where pipelines live, the file format, how a run moves
through stages, the template variables, and a complete worked example. The
contract behind it is [ARCHITECTURE.md §8](ARCHITECTURE.md#8-pipelines).

## Where pipelines live

Archon looks in three places. A later source replaces an earlier one **by
name, as a whole directory**:

| Precedence | Location | Visible to |
| --- | --- | --- |
| 1 (lowest) | Built-in: `pipelines/<name>/` in the Archon repository, compiled into the binary | everything |
| 2 | User: `$ARCHON_CONFIG_DIR/pipelines/<name>/` (default `~/.config/archon/pipelines/<name>/`) | everything |
| 3 (highest) | Repository: `<repo>/.archon/pipelines/<name>/` | items in that repository only |

- A directory counts as a pipeline only if it contains `pipeline.toml`. Other
  directories, loose files and hidden entries (`.name`) are ignored. A
  `pipeline.toml` that exists but cannot be read (a dangling symlink, a
  directory, missing permissions) still counts, and is reported as broken.
- An override replaces the **whole** pipeline. It inherits no stages or
  prompts from the pipeline it shadows. To tweak a built-in, copy its
  directory from `pipelines/` and edit the copy.
- A pipeline that fails to parse or validate is left out of the catalog and
  reported with its path and the reason. It **never falls back** to a
  lower-precedence pipeline of the same name: the name stays unavailable
  until you fix the file, so a typo cannot silently run different stages.
  `archon pipelines check` and the orchestrator's `list_pipelines` both show
  these problems.
- A repository can hide pipelines from its items with
  `<repo>/.archon/config.toml`:

  ```toml
  [pipelines]
  disabled = ["quick-pr"]
  ```

## Quick start: add a pipeline

The repository ships a complete example,
[`docs/examples/pipelines/bugfix-tdd/`](examples/pipelines/bugfix-tdd/). It
reproduces a bug with a failing test, fixes it, gets two independent reviews
with a fix loop, and ships. To install it for every repository:

```sh
mkdir -p ~/.config/archon/pipelines
cp -r docs/examples/pipelines/bugfix-tdd ~/.config/archon/pipelines/
archon pipelines check        # lists it, or explains what is wrong
```

To make it available to a single repository only, copy it to
`<repo>/.archon/pipelines/bugfix-tdd/` instead. That copy wins over a user
pipeline of the same name.

The orchestrator now sees `bugfix-tdd` in `list_pipelines`, with its stages,
gates and nudges, and can enqueue items with it. A test in
`crates/archon-pipeline/tests/catalog.rs` copies this exact directory into a
temporary user directory and runs it through the engine, so the example
cannot rot.

The orchestrator may also write a pipeline itself when you keep asking for
the same shape of work. It saves one to the user pipelines directory only
after `archon pipelines check` passes, tells you its name, and never
overwrites a pipeline it did not write. A repository pipeline
(`<repo>/.archon/pipelines/`) it only proposes, and adds as an item once
you agree.

## `pipeline.toml`

```toml
[pipeline]
name = "bugfix-tdd"          # required; must equal the directory name; [a-z0-9-]+
description = "Reproduce, fix, review twice, ship"   # required; shown to the orchestrator
version = 1                  # required; the schema version, only 1 exists
workspace = "worktree"       # worktree (default) | repo-readonly | scratch
requires_repo = true         # default true; false lets items run without a repository

[pipeline.review]            # optional; see "A declared review"
level = "standard"           # only standard may be declared
stage = "build"              # the delegating work stage that holds the review

[nudges]                     # optional advice to the orchestrator; never enforced
tier = "balanced"            # fast | balanced | strongest
independent_reviewers = true # prefer distinct models or vendors across reviewers
notes = "When to use this pipeline."

[[stage]]                    # one or more, run in order
id = "review"                # required; [a-z0-9-]+, unique
kind = "review"              # required; propose | work | team | review | test | ship | report
prompt = "review.md"         # required; a file directly in this directory
fan_out = 2                  # default 1; parallel agents, 1..=16 (a team's is its roles)
gate = "all_pass"            # default none; none | all_pass | majority | any
timeout_minutes = 60         # default 120, 1..=43200 (30 days); Archon cancels the agent after this
allow_delegation = false     # true lets the stage agent delegate, whatever the item's tools
nudge = "Advice for this stage's model choice."
on_fail = { goto = "fix", max_loops = 2 }   # optional fix loop

[[stage]]                    # a team stage (see "Team stages")
id = "build"
kind = "team"
prompt = "build.md"
roles = ["api", "ui"]        # required on team stages; 2 to 8, [a-z0-9-]
role_prompts = { ui = "Own the CSS; never touch src/api/." }   # optional, per role
```

Unknown keys are errors, so a misspelt field cannot be silently ignored.

### Workspace modes

| Mode | The stages work in | Use it for |
| --- | --- | --- |
| `worktree` | A fresh git worktree and branch per run, forked from the repository's base branch. All stages of the run share it, and a fix loop reuses it. A team stage's roles each get a worktree of their own beside it (see "Team stages"). | Anything that changes code. Requires `requires_repo = true`. |
| `repo-readonly` | A snapshot: a detached worktree of the repository's current `HEAD` commit, made for the run and removed with `--force` when it ends, however it ends (`archon gc` collects one left behind). Uncommitted changes in the operator's checkout are not in it, and nothing written there reaches that checkout. It is read-only by instruction only; nothing enforces it. Without a repository, a scratch directory. | Questions and analysis (`ask`). No `propose`, `report` or `ship` stages, because they write into the workspace and nothing in it is kept. |
| `scratch` | An empty per-item directory, kept after a non-shipping success until `worktrees.gc_after` (or an explicit gc cutoff). Published HTML and relative assets normally have durable copies under `<data>/reports/<report-id>/`, independent of workspace retention; if copying fails, the original page is published with a warning and shares workspace retention. | Work that needs no repository. |

### Validation rules

Archon checks these when it loads a pipeline. Each error names the stage and
the field:

- `name` equals the directory name and matches `[a-z0-9-]+`. `version` is 1.
- There are 1 to 32 stages. Stage ids are unique and match `[a-z0-9-]+`.
- Each `prompt` is a plain file name (no `/`, no leading `.`) that exists in
  the directory. Symlinks are followed only while they stay inside the
  pipeline directory. Files are limited to 256 KiB.
- `fan_out` is between 1 and 16. A value above 1 is refused for `work` and
  `ship` stages, because parallel agents would write to one worktree.
- A `team` stage lists 2 to 8 `roles`. Each is unique across the pipeline,
  1 to 32 characters of `[a-z0-9-]`, starts and ends with a letter or digit
  and has no `--`. Leave `fan_out` out, or set it to the number of roles.
  `role_prompts` may only name listed roles. A team stage has no `gate`,
  and it needs `workspace = "worktree"`. `roles` and `role_prompts` are
  refused on other kinds.
- A `gate` other than `none` is only allowed on `review`, `test` and `report`
  stages. `review` and `test` stages must have one (`all_pass`, `majority`
  or `any`): their agents return a verdict, and `gate = "none"` would ignore
  it.
- `on_fail.goto` names an **earlier** stage, and `max_loops` is at most 10.
- There is at most one `ship` stage, it is the last stage, and the pipeline
  uses `workspace = "worktree"`.
- There is at most one `propose` stage, and it is the first stage.
- `propose` and `report` stages need `workspace = "worktree"` or
  `"scratch"`: they write their pages under `.archon/reports/` in the
  workspace.
- `workspace = "worktree"` requires `requires_repo = true`.
- `[pipeline.review]`, when present, declares `level = "standard"` and
  names a `work` stage with `allow_delegation = true`. `strict` and `none`
  are refused there.
- `timeout_minutes` is between 1 and 43200 (30 days). The stage agent's
  deadline is exactly that long after it starts: a larger value is refused,
  never shortened.
- Every `{{placeholder}}` in a prompt is a known variable (see below). The
  `team.*` variables are only allowed in a team stage's prompt.

### Delegation

A stage agent can delegate when its stage sets `allow_delegation = true`
or its item runs with `tools = "yolo"`. Otherwise it is a leaf and has no
`delegate` tool. Delegation changes nothing about Archon's own tools: a
guarded stage still sees only messaging, and no stage agent ever sees the
tools that change the queue.

An agent that can delegate also gets the rest of Aether's tools, with
three exceptions that Archon hides from every stage agent:
`create_schedule`, `update_schedule` and `list_schedules`. A schedule
would start agents after the stage has ended, outside the run. In a
`repo-readonly` workspace `integrate_workspace` is hidden too.

A child inherits none of its parent's instructions: Aether gives it only
the `objective` and `instructions` passed to `delegate`. Neither the stage
contract nor your stage prompt reaches it. Archon therefore adds a
`## Delegation` section to the contract of every agent that can delegate,
by either route. It says to:

- let a child delegate once more when its part splits further, telling it
  to pass `leaf: true` for its own children, and pass `leaf: true` for a
  child that should not. The watchdog cancels an agent a third level down;
- give a child that edits its own copy (`workspace_mode: "isolated"`) and
  bring back only accepted changes with `integrate_workspace`, while a
  child that only reads shares the stage's workspace. In a read-only
  workspace every child shares it and only reads;
- choose each child's model and effort within the model policy that
  applies to the item, which the section states in words. The watchdog
  cancels a child outside it;
- wait for every child or cancel it before finishing;
- delegate a new child for a second look, because a finished one cannot
  be asked again;
- hand a child what the stage agent already holds instead of having it read
  again, never the conversation, and hand a reviewer the exact base and head
  commits, the changed paths, the checks run with their results and the
  commit they ran on, the known risks and the repository guidance;
- state, in every child's `instructions`, the rules for its role: stay in
  the workspace it is given and keep off other worktrees and the
  operator's checkout, do not push, never override the git identity, ask
  the operator nothing, and read only for a reviewer and for every child
  of a read-only stage.

A stage prompt that delegates should name any further constraint its
children must be given, as `adaptive`'s `build.md` does.

**When the stage ends.** Aether does not stop an agent's children when
the agent completes or fails its task. Archon does: once a stage agent
that could delegate has settled and its own turn has ended (or 60 seconds
have passed), Archon cancels it, which asks every agent still running
beneath it to stop. Aether stops them cooperatively, so Archon then waits
until Aether holds none of them as running. The run's next stage, or the
replacement of a failed agent, starts only after that. The stop is stored
before the stage settles, so it survives a daemon restart, and it stays
owed while Aether is unreachable. If the agents have not ended two
minutes after the cancel, a blocker names them; the item keeps waiting
and goes on by itself once they end.

Aether's isolated workspaces are Git worktrees sharing the repository's
object store. Their snapshot includes tracked and non-ignored untracked
files; ignored build outputs and caches are not copied. Large source
trees, unignored artifacts and child builds still consume disk, so keep
outputs ignored and size concurrency to the work and the machine.
Aether's daemon keeps these worktrees beside its database, outside the
source repository; Archon has no setting for it.

### A declared review

The `review` autonomy knob normally follows a pipeline's gates: gated
`review` or `test` stages make it `standard` or `strict`, and a pipeline
without them runs at `none`. A pipeline whose review happens inside one
agent's stage has no such gate, so it declares the review instead:

```toml
[pipeline.review]
level = "standard"
stage = "build"
```

The pipeline then runs at `review = standard`: an item's knob, a
repository's ceiling and the stored autonomy all use the declared level.
Only `standard` can be declared, and `stage` must be a `work` stage with
`allow_delegation = true`, because its reviewer is an agent it delegates
to.

Archon holds the stage to it. The contract of that stage gains a
`## Review on record` section, and its `complete_task` must carry one of:

```json
{"review": {"verdict": "pass", "reviewer_model": "<provider>/<model>"}}
{"review": {"verdict": "skipped", "reason": "<why this change needed none>"}}
```

When the object is missing, the verdict is anything else, or a pass names
no reviewer, Archon fails the run at that stage and nothing ships.

A pass is not taken on the agent's word. As the stage ends, Archon asks
Aether which agents that agent delegated to. The pass counts only when one
of its direct children completed, neither cancelled nor failed, on the
model `reviewer_model` names; otherwise the run fails at that stage and the
error lists what the agent did delegate to. Archon records that child's
agent id with the evidence. This shows that such a reviewer ran to
completion, not what it read or concluded. A skip has no reviewer to
check: its reason is shown with the stage in `archon queue show` and the
orchestrator's `get_item`, and `ship` in `adaptive` reads the diff before
shipping one.

On a pass or a justified skip Archon appends
`REVIEW: pass, by <model> (agent <id>)` or `REVIEW: skipped: <reason>` as
the last line of the stage summary, which the later stages receive.

## How a run moves

The run state machine is `RunDriver` in `crates/archon-pipeline`:

1. **Spawn.** Entering a stage starts `fan_out` agents. Each gets the rendered
   stage prompt as its task, plus the stage contract (below) as its
   instructions. The model and effort for each agent come from the item's
   `model_plan`. When the plan lists fewer choices than slots, Archon cycles
   through them.
2. **Wait.** Nothing is decided until every agent of the stage has finished.
3. **Gate.**
   - `none`: the stage passes when every agent completed. A failed, timed-out
     or cancelled agent fails it. Only for stages without a verdict: `review`
     and `test` stages must gate.
   - `all_pass`: every agent must return verdict `pass`.
   - `majority`: more than half must pass (2 of 3, 3 of 5, and 2 of 2).
   - `any`: at least one must pass.

   A missing or unreadable verdict counts as `fail`, and so does an agent
   that failed, timed out or was cancelled.
4. **Advance.** When a stage passes, the next stage starts. After the last
   stage, the run succeeds, and its summary is that stage's summaries joined.
5. **Loop.** When a stage fails and has `on_fail`, and that stage has looped
   fewer than `max_loops` times, the run goes back to the `goto` stage. That
   stage's next attempt receives the failing agents' summaries and findings
   as `{{feedback}}`. A prompt that does not show `{{feedback}}` gets it
   appended under `## Feedback to address`, so feedback is never lost. Each
   stage counts its own loops. When the loops run out, or the stage has no
   `on_fail`, the run fails at that stage.
6. **Blocked.** An agent that fails with an error starting `BLOCKED:` ends
   the run immediately. There is no loop, its live siblings are cancelled, and
   the operator gets a blocker in chat. Use `BLOCKED:` for "a human must act",
   such as a missing credential or an ambiguous requirement.
7. **Operator gate.** When the item's knobs make a passed `propose` or
   change-report stage wait for the operator, the run holds there. No agent
   runs while it waits. Approving moves on. Feedback re-runs the proposal, or
   for a change report every stage from the first `work` stage, with the
   feedback as `{{feedback}}` (appended as in step 5 when the prompt does not
   show it), at most 5 times per gate. A revision is a fresh round of work:
   every stage it re-runs starts again with its full `max_loops` fix-loop
   budget, whatever earlier rounds used. Rejecting cancels the item.

`attempt` counts how often a stage has been entered: 1 the first time, 2
after the first loop back to it, and so on. Each stage has its own count. In
`strict-pr-review`, for example, the test stage's third attempt can lead to
the review stage's second.

Every decision is derived from the stage runs Archon has persisted, so a
daemon restart resumes a run exactly where it was.

## Team stages

A `team` stage runs one agent per role, in parallel. Each role writes in a
git worktree of its own, and when every role passed, Archon merges the
roles' branches onto the run branch. The run still ships one pull request.

- **Layout.** A pipeline with a team stage lays its run out as
  `<worktrees>/<repo>/<item>/main` (the run worktree, where every other
  stage works) and `<worktrees>/<repo>/<item>/<stage>-<role>` for each
  role, beside it and never inside it. The run's Aether node is scoped to
  `<item>`, so it covers both. Pipelines without a team stage keep the
  single `<item>` worktree. An item whose `<item>` directory already holds
  such a worktree cannot start a team pipeline until it is removed, and
  the other way round.
- **Branches.** Each attempt of a role works on
  `<run branch>--<role>-<run8>-a<attempt>`, where `<run8>` is the tail of
  the run id, forked from the run branch as it is when the attempt starts.
  A retried item's new run therefore forks new branches too. A retried
  agent reuses its worktree as is. A new attempt switches the worktree to
  the new branch. First, a rebase, cherry-pick, revert, bisect, `git am` or
  merge the previous attempt left in progress is ended without touching
  its files, whatever it left uncommitted is committed onto that attempt's
  branch, and a detached `HEAD` is kept as `refs/archon/saved/<commit>`.
  If the role checked out a branch that is not one of its own, such as
  `main`, that branch never moves: the work is committed on a detached
  `HEAD` and kept the same way. That work is kept but never merged. An
  attempt's branch that already exists is reused only when it holds the
  run branch; one that does not is not this run's, and the slot fails
  naming it. A role's worktree deleted by hand is added again, and its
  git directory (index, reflogs, the git directories of submodules added
  in it) is moved to `<repo>/.git/archon-set-aside/<name>-<n>` rather than
  deleted, a detached `HEAD` kept as above, and posted to the run's
  progress.
- **Admission.** A team stage's roles start together or not at all: the
  dispatcher admits every role's agent at once, or waits. While it waits,
  other work under the caps it waits on (the global cap, its repository's
  or its pipeline's) waits behind it, so smaller work cannot starve it;
  work under other caps still starts. Review and report fan-outs are
  still admitted partially. An item whose pipeline has a team stage with
  more roles than `max_parallel_agents`, the repository's `max_per_repo`
  or the pipeline's `max_per_pipeline` allow is refused at enqueue and at
  retry, naming the stage, its role count and the cap. If the caps are
  lowered below a team's size afterwards, it waits without holding back
  other work, and the operator gets one blocker naming the team stage (or
  the queued item whose first stage it is), its size and the cap.
- **Roster.** Once every role of an attempt has an agent, Archon sends
  each running role one Aether message, the roster:

  ```text
  Archon team roster v1 for team stage `build` attempt 1. You are role `api`.

  - `api`: agent `agt_…`, worktree `<item>/build-api`, branch `<run branch>--api-…-a1`
  - `ui`: agent `agt_…`, worktree `<item>/build-ui`, branch `<run branch>--ui-…-a1`

  Message a peer with Aether's `send_message` and its agent id. This roster
  supersedes any earlier version.
  ```

  The version starts at 1 and grows by one each time a role's agent is
  replaced (after an infrastructure failure), and the new version goes to
  every role still running; one that finished gets none. Each send is
  keyed by the recipient's stage run and the version, so a restart or a
  lost reply never delivers a version twice. A send Aether cannot take
  now is retried by the next sweep.
- **A failed role.** When a role fails, times out, is cancelled or runs
  out of budget, its siblings still running are cancelled at once, their
  agents stopped, and the reason (`cancelled because role `api` failed in
  team stage `build` attempt 1`) is recorded on each and posted to the
  run's progress, even when that failure also ends the run. Roles that already completed are left as they are. The
  stage then fails, and its feedback names the failed role, through
  `on_fail` like any failed stage. A stop Aether does not confirm is
  retried by the sweep, across restarts.
- **Commit.** Only a role's branch is merged. The contract tells each
  role to commit on its branch, to stay in its worktree and its role, and
  not to push. What a passed role left uncommitted, untracked files
  included, is committed onto its branch before the merge, as
  `archon: commit the work role <role> left uncommitted`. These commits
  run no repository hook, so they are in the pull request's history for
  review to see, and each is posted to the run's progress naming its
  files, so a stray `.env` does not go unnoticed. Repositories nested in
  the worktree and changes to tracked files inside submodules are never
  committed this way; untracked files inside a submodule, such as its
  build output, are left alone. Roles fork from the run
  branch's last commit, so the contract also
  tells the pipeline's work stages to commit on the run branch. A role
  does not start while the run worktree has uncommitted changes to tracked
  files: its slot fails with the files named, which loops through
  `on_fail` like any failed stage.
- **Merge.** The stage passes when every role completed; a failed role
  fails it as above, and nothing is merged. Archon then runs
  `git merge --no-ff` of each role's branch in `roles` order, in the run
  worktree. A branch already on the run branch is skipped, so a merge that
  a restart interrupted resumes without merging anything twice. The
  outcome is persisted before the run moves on.
- **Refusal.** A role whose worktree has another branch checked out (it
  committed elsewhere), holds an unfinished merge, rebase or other git
  operation, or holds a nested repository or changes inside a submodule,
  may have work that would not be merged. So may one holding a submodule
  whose git directory is not one `git submodule` made (a repository the
  role created in place): git would run the filters its config names, so
  Archon runs no git that enters it. In the run worktree, such a
  submodule stops the team stage with an error. Nothing more is merged then, and
  the stage fails with the reason as `{{feedback}}`, through `on_fail` as
  for a conflict. The next attempt starts from a clean branch as above.
  A role's worktree holding such a submodule, or whose `HEAD` names a
  branch with no commit (what it left belongs to no commit), is not
  switched to it but moved aside, untouched, to
  `<item>/.set-aside/<stage>-<role>-<n>`, its git directory (index,
  reflogs, and the git directories of submodules it added with
  `git submodule add`) to `<repo>/.git/archon-set-aside/<name>-<n>`, and
  the move is posted to the run's progress; the attempt starts in a fresh
  worktree. Its commits stay on the earlier branch (a detached `HEAD` is
  kept as above), and both directories are yours to inspect and remove.
  Git reuses the git directory's old name for the fresh worktree, so
  every submodule gitfile in the kept worktree that named it is pointed
  where it is kept, and each submodule git directory in it has its
  `core.worktree` pointed into the kept worktree, or removed when it
  named anything else. The kept worktree gets no `.git`: one would let
  any git you run there apply the filters a role planted. The progress
  notice says how to look at it once you have checked its submodules'
  config: `git --git-dir=<kept git directory> --work-tree=<kept worktree>
  status`. Nothing is moved when `.set-aside` or `archon-set-aside` is a
  symlink rather than a directory; the slot fails instead.
- **Conflict.** A conflicting merge is aborted and fails the stage. Its
  headline and the conflicted files become the `{{feedback}}` of the
  `on_fail` target, so an earlier stage can re-plan the split. Untracked
  files in the run worktree that a role's branch adds, or adds a file or a
  directory in the way of, count as conflicts too. Without `on_fail`, or
  when its loops are used up, the run fails. Roles merged before the
  conflict stay on the run branch. The feedback of a conflict or a refusal
  lists every role's branch, and whether it was merged, since the next
  attempt forks afresh from the run branch.
- **Budget.** Each role's agent gets an even share of what is left of the
  item's budget when that agent is reserved: the remainder divided by the
  number of roles. A role's agent replaced after an infrastructure failure
  gets what its predecessor left of that share.
- **Summaries.** The stage's summary joins the roles' summaries, labelled
  `[role api]`, `[role ui]`.

The merges are commits the daemon makes: no repository hook runs for
them, and they use the git identity of the daemon's user, or
`Archon <archon@localhost>` when git has none. They follow the
repository's `commit.gpgsign`, and the daemon waits for them without a
timeout, so signing must not prompt for a passphrase. A git failure other
than a conflict or a refusal fails the run as a blocker. Worker worktrees and branches stay
after the run. Worktree gc, which `archond` runs itself and `archon gc`
runs on demand, deletes their build caches after `worktrees.cache_after`,
then removes the run's worktrees (`main` and each `<stage>-<role>`,
keeping one with uncommitted changes unless `--force`) and what was set
aside for it in `<item>/.set-aside/` and `<repo>/.git/archon-set-aside/`,
once the item is terminal and older than `worktrees.gc_after` or
`--older-than`, then the `<item>` directory once that leaves it empty.
Branches stay.

A role can rewrite its worktree's `.git` file, or replace the worktree
with a symlink, to point git at another checkout. Before running git in
a role's worktree or the run worktree, Archon checks that the directory
is exactly the worktree git registered there: not a symlink, of the
item's repository, and with git's administrative directory for that
worktree and no other. Otherwise the slot, or the merge, fails without
touching anything. No repository hook runs in any git Archon runs in a
team's worktrees.

## Stage prompts and template variables

A stage prompt is Markdown and becomes the agent's task objective.
Placeholders are replaced literally. There is no logic and no escaping.
Values are inserted as-is and never scanned again, so a brief that contains
`{{` is safe. Write `{{{{` to get a literal `{{`.

| Variable | Value |
| --- | --- |
| `{{task.title}}`, `{{task.brief}}` | The queue item's title and full brief. The brief is all the agent knows about the request, so always include it. |
| `{{repo.name}}`, `{{repo.path}}` | The registry entry. Empty when there is no repository. `repo.path` is the operator's checkout, not the workspace. |
| `{{workspace}}` | The absolute path the agent works in. |
| `{{branch}}`, `{{base_branch}}` | The run branch and the branch it was forked from. |
| `{{ship.mode}}` | `pr` or `local`. |
| `{{ship.auto_merge}}` | `true` when the item's `merge` knob is `auto`, else `false`. Only the operator can allow `merge = auto` ([Autonomy levels](guide.md#autonomy-levels)). |
| `{{stage.id}}`, `{{attempt}}` | The current stage and its attempt number (1-based). |
| `{{fan.index}}`, `{{fan.count}}` | This agent's slot (**1-based**, so "reviewer 2 of 3") and the stage's `fan_out`. |
| `{{team.role}}`, `{{team.roles}}` | Team stages only: this agent's role and every role, comma-separated. |
| `{{team.branches}}` | Team stages only: every role's branch for this attempt, comma-separated. In a team stage, `{{workspace}}` is the role's own worktree. |
| `{{run.id}}`, `{{item.id}}` | Archon ids. |
| `{{feedback}}` | The failing agents' summaries and findings from the gate that looped back to this stage, or the operator's feedback when a revision re-runs it. Empty on the first attempt. |
| `{{previous.<stage-id>.summary}}` | The latest summary of an **earlier** stage. Fan-out summaries are joined and labelled `[#1]`, `[#2]`; a team's are labelled `[role api]`. |

An unknown placeholder, a `previous.*` reference to the same or a later
stage, and an unterminated `{{` are all load-time errors.

### The stage contract

Archon adds a **stage contract** to every agent as its instructions, so
pipeline authors cannot forget it. The contract depends on the stage kind:

- **Every stage.** Work only in the workspace, and never ask the operator
  anything. Finish with Aether's `complete_task` and a concise summary. Fail
  with `fail_task`, starting the error with `BLOCKED:` when a human must act.
- **Review and test stages, and any gated stage.** The result carries
  `structured` data `{"verdict": "pass"|"fail", "findings": [{"severity",
  "title", "detail", "path"}]}`, and the summary ends with `VERDICT: PASS` or
  `VERDICT: FAIL`. Archon reads the structured verdict first, then the last
  `VERDICT:` line.
- **Ship stages.** `structured` includes `{"pr_url": …}` or `{"commit": …}`.
  The commits a ship stage is given are delivered as they are: it does not
  amend, rebase, squash or reset them. It may reword a commit message that
  breaks a repository rule, and then shows that `git rev-parse HEAD^{tree}`
  did not change.
- **Propose stages.** Read, but change nothing. The proposal goes to
  `<workspace>/.archon/reports/proposal/index.html`, `structured.reports`
  lists it, and the summary states the approach on its own, because the
  operator may approve it from the summary.
- **Report stages.** HTML goes under
  `<workspace>/.archon/reports/<name>/index.html`, and
  `structured.reports` lists the files for Archon to publish (in Lavish, when
  it is installed).
- **Team stages.** The agent is told its role and the other roles, to
  change only its own worktree and never a peer's, to stay within its
  role, and to commit on its branch without pushing. It is told a roster
  will arrive (a later version supersedes an earlier one), and to use
  `send_message` with a peer's agent id to agree interfaces early, to
  report a breaking change, and to ask for help debugging, in concise
  messages. It must never block on a peer for long: it proceeds on its
  best assumption and states that assumption in its summary. A role's
  `role_prompts` entry is appended to its prompt under `## Your role`.
- **Read-only workspaces** and **fan-out stages** get an explicit "do not
  modify files" rule.
- **An agent that can delegate** gets the `## Delegation` section (see
  "Delegation"), and **the stage a declared review names** gets
  `## Review on record` (see "A declared review").
- **Every agent of a worktree item queued after others** (`depends_on`)
  gets a `## Dependencies` section: one JSON object per dependency,
  `{"item":"qi_…","branch":"…","pull_request":"…"}`, and the rule for a
  change built on one of those branches. The item's own branch still forks
  from `{{base_branch}}`; a change stacked on a dependency's branch has
  `git merge-base <branch> HEAD` as its base where `{{base_branch}}` does
  not contain that commit, and is diffed, reviewed and reported from there.
  The contract overrides a prompt that says to diff from `{{base_branch}}`.

Your prompt therefore says *what* to do. It does not need to repeat how to
finish.

### Writing a good stage prompt

- **Give a role, the inputs and the goal in the first paragraph.** Say who
  the agent is, what it receives, and what "done" means.
- **Include `{{task.brief}}`.** Stage agents see nothing of the conversation
  that led to the item.
- **Wire the stages together.** Hand earlier results forward with
  `{{previous.<id>.summary}}`, and tell each stage what its summary must
  contain for the stages after it.
- **Show `{{feedback}}` in every `goto` target and in the first `work`
  stage**, and tell the agent to deal with every finding. The first work
  stage is where a change-report revision sends the operator's feedback.
  Without the placeholder, Archon appends the feedback at the end of the
  prompt, where the agent may give it less weight.
- **For gated stages, state the verdict rule precisely.** For example: "fail
  if any critical or high finding". A vague rule makes the gate flaky.
- **For fan-out stages, give each slot a lens with `{{fan.index}}`.** For
  example, reviewer 1 checks correctness, reviewer 2 security and reviewer 3
  design. Parallel agents then find different problems instead of the same
  one three times.
- **Never name a model.** Model and effort are chosen per item by the
  orchestrator within the operator's allowlist. Put advice in `[nudges]` and
  stage `nudge` fields instead ("prefer the strongest tier", "a fast model is
  enough"), and the orchestrator weighs it against the allowlist.

## Stages the autonomy knobs add

Two of an item's autonomy knobs need stages that most pipelines do not
declare (ARCHITECTURE §18). Archon inserts them into the run's copy of the
pipeline:

| Knob | Inserted stage | Where |
| --- | --- | --- |
| `proposal = lavish` | `propose`, kind `propose`, prompt `pipelines/_shared/propose.md` | First. The run then waits for the operator to approve the direction. |
| `report = with-pr` or `before-pr` | `report`, kind `report`, prompt `pipelines/_shared/report.md` | Right before `ship`. With `before-pr` the run waits for the operator before shipping. |

Both knobs apply only to pipelines with a `ship` stage. An inserted stage has
`fan_out = 1`, no gate, a 45-minute timeout and `allow_delegation = false`. If the item's
`model_plan` does not name it, it gets the first choice of the pipeline's
first stage.

**Declaring your own.** A pipeline that declares a `propose` stage, or a
`report` stage before its `ship` stage, gets no duplicate. The knobs gate
your stage instead, and when several report stages come before `ship`, the
last one is the change report. Your propose stage must be first, and it
should write the page and summary its stage contract asks for. Without
`proposal = lavish`, a declared propose stage still runs, but nothing waits
for the operator. The stage ids `propose` and `report`, and the prompt file
names `archon-propose.md` and `archon-report.md`, are reserved for the
inserted stages. A pipeline that uses them for something else is refused
when a knob needs the stage.

Later stages get extra context in their prompts:
- Every stage after `propose` gets the proposal's summary, whether the
  operator approved it, and the operator's note.
- The inserted report stage gets every earlier stage's summary.
- The ship stage gets the change report's summary, and puts it in the pull
  request body under `## Change report`.

**Proposal and change-report pages.** The shared prompts scale page detail to
the task. Both pages always state the goal, approach or decision, risk,
acceptance check, and open operator decisions (including when there are none).
For a trivial task, those points should fit in a few lines. Alternatives,
file-by-file detail, design rationale, diagrams, verification evidence and
follow-ups are added when they help explain the work, material risk, or open
findings. Page paths, gate behavior and report metadata do not change.

## Worked example: `bugfix-tdd`

The built-in `bugfix` uses this reproduce → fix → review → ship pattern
with one independent reviewer and up to two fix loops; no installation is
needed. This `bugfix-tdd` tutorial keeps two reviewers to demonstrate fan-out.
Use `quick-pr` for trivial obvious one-liners and `strict-pr-review` for
security or concurrency bugs.

[`docs/examples/pipelines/bugfix-tdd/pipeline.toml`](examples/pipelines/bugfix-tdd/pipeline.toml):

```toml
[pipeline]
name = "bugfix-tdd"
description = "Reproduce a bug with a failing test, fix it, two independent reviews, then ship"
version = 1
workspace = "worktree"
requires_repo = true

[nudges]
tier = "balanced"
independent_reviewers = true
notes = "For bug reports with a clear symptom. A failing test is committed before any fix."

[[stage]]
id = "reproduce"
kind = "work"
prompt = "reproduce.md"
timeout_minutes = 60
nudge = "A careful model at medium effort; it only writes a test."

[[stage]]
id = "fix"
kind = "work"
prompt = "fix.md"
timeout_minutes = 120
nudge = "A strong coding model at high effort."

[[stage]]
id = "review"
kind = "review"
prompt = "review.md"
fan_out = 2
gate = "all_pass"
timeout_minutes = 60
nudge = "Two reviewers on different models when the allowlist permits."
on_fail = { goto = "fix", max_loops = 2 }

[[stage]]
id = "ship"
kind = "ship"
prompt = "ship.md"
timeout_minutes = 90
nudge = "A fast model at low effort."
```

What happens for one item:

1. `reproduce` (attempt 1) commits a failing test. Its summary names the test
   and the failure.
2. `fix` (attempt 1) sees that summary through
   `{{previous.reproduce.summary}}`, fixes the root cause and commits.
3. `review` starts two agents. They may run on different models, because
   the orchestrator follows `independent_reviewers`. Both must pass.
4. If a reviewer fails the change, `fix` runs again (attempt 2), and
   `{{feedback}}` holds that reviewer's summary and findings. Then `review`
   runs again (attempt 2). After two fix loops, a third failing review fails
   the run.
5. `ship` pushes and opens the PR or commits locally, depending on the
   repository's ship mode.

A matching `model_plan` for `enqueue` names every stage. Each fan-out stage
may list one choice per agent:

```json
{"reproduce": [{"model": "claude-code/claude-opus-5-5", "effort": "medium"}],
 "fix":       [{"model": "claude-code/claude-opus-5-5", "effort": "high"}],
 "review":    [{"model": "claude-code/claude-opus-5-5", "effort": "high"},
               {"model": "codex/gpt-6-astra", "effort": "high"}],
 "ship":      [{"model": "claude-code/claude-opus-5-5", "effort": "low"}]}
```

Archon refuses a plan that leaves a stage out, that names a stage the
pipeline does not have, or that lists more choices than a stage has agents.

## Overriding a built-in

To change a built-in pipeline, for example to give `default-pr-review` two
reviewers:

```sh
cp -r pipelines/default-pr-review ~/.config/archon/pipelines/
$EDITOR ~/.config/archon/pipelines/default-pr-review/pipeline.toml   # fan_out = 2
archon pipelines check
```

Your copy now replaces the built-in everywhere. Delete the directory to go
back to the built-in.

## Built-in pipelines

| Name | Stages | Nudges |
| --- | --- | --- |
| `adaptive` | build (work, delegation enabled, holds the declared review) → ship | balanced tier; `review = standard`. One lead chooses workers at runtime and has the integrated change reviewed once. A trivial or mechanical change may skip the review with the reason on record. |
| `quick-pr` | implement → ship | fast tier. For small, low-risk changes. The implement stage refuses risky work. |
| `default-pr-review` | implement → review (1, `all_pass`, back to implement ×2) → ship | balanced tier, independent reviewer |
| `bugfix` | reproduce (failing test) → fix → review (1, `all_pass`, back to fix ×2) → ship | balanced tier, independent reviewer. For bugs with a describable symptom; `quick-pr` for trivial one-liners, `strict-pr-review` for security/concurrency bugs. |
| `docs` | write → verify (1, `all_pass`, back to write ×2) → ship | fast/balanced tier, independent reviewer. For README, guides, API/reference docs, changelogs and docstrings; verifies every claim, command and internal link against the code. Use `quick-pr` for a typo. |
| `dependency-upgrade` | survey → upgrade → verify (`all_pass`, back to upgrade ×3) → ship | balanced tier. Research changelogs, bump libraries/toolchains/base images, verify build/tests/audit. Use `default-pr-review` for features that add dependencies. |
| `refactor` | baseline/characterize → refactor → verify (`all_pass`, back to refactor ×3) → review (1, `all_pass`, back to refactor ×2) → ship | balanced tier, independent reviewer. Structure-only changes; use `default-pr-review` for behaviour changes. |
| `strict-pr-review` | implement → test (`all_pass`, back to implement ×3) → review (3 lenses, `all_pass`, back to implement ×3) → ship | strongest tier, independent reviewers |
| `team-pr` | plan → build (team: `backend`, `frontend`) → e2e (`all_pass`, back to build ×2) → review (1, `all_pass`, back to build ×2) → ship | balanced tier, independent reviewer. For a change that spans cleanly separable parts. See below. |
| `performance` | measure → optimize → verify (`all_pass`, back to optimize ×3) → review (1, `all_pass`, back to optimize ×2) → ship | strongest tier, independent reviewer. For a specific slow path with a measurable goal, not speculative micro-optimization; use `default-pr-review` for ordinary changes. |
| `test-coverage` | analyze → write → verify (test, `all_pass`, back to write ×2) → ship | balanced tier. Raise coverage of existing code with risk-ranked tests, at least three passing runs and per-test mutation checks. Bugs become follow-ups; use `bugfix` to fix one. |
| `agent-testing` | plan → exercise (2 exploratory testers) → report (HTML) | balanced tier, independent testers |
| `code-audit` | scope → audit (3 lenses, report) → report (HTML with issue drafts) | strongest tier, independent auditors. Read-and-report only; findings become follow-up items. Use `ask` for a single question or `agent-testing` to exercise the product. |
| `e2e-testing` | setup → e2e (`all_pass`, back to setup ×1) → report (HTML with screenshots) | balanced tier |
| `ask` | answer | balanced tier. Read-only, and no repository required. With one, it works in a disposable snapshot of the repository's `HEAD` commit, so it sees committed work only. |
| `errand` | errand (report) | balanced tier. A small job answered in text or as a Lavish page, with no repository changes or outward side effects. Scratch workspace, repository optional. Use `ask` for repository questions in text, and a shipping pipeline for changes. |
| `design-doc` | research → critique (`all_pass`, back to research ×2) → report (HTML design/RFC) | strongest tier, independent critic. Before a large or ambiguous change; no product-code changes or shipping. Use `proposal: lavish` for design and implementation in one item. |

Their prompts, in [`pipelines/`](../pipelines/), are a good starting point
for your own. Each pipeline directory stays self-contained, so the
shipping pipelines carry their own copy of `ship.md`. Everything from
`## Mode \`pr\`` to the end (the delivery rules for each ship mode) must be
identical in all shipping pipelines, and a test in
`crates/archon-pipeline/tests/validate.rs` fails when the copies drift. Edit all
copies together.

### `adaptive`

Use `adaptive` for a repository change where one lead that keeps its
context through fixes, or integrates parts that split, saves restarts. A
small low-risk change is cheaper in `quick-pr`, and a bounded change by
one author with one review in `default-pr-review`. The lead sizes,
implements, integrates, verifies and fixes the change. Its prompt has it
spend where spending pays:

- **Workers.** A leaf child in an isolated workspace gets an independent
  deliverable with its own paths, and only when running it in parallel
  saves more than briefing it, integrating it and the reading it repeats.
  Often that is no worker at all, and a bounded local read is never
  delegated.
- **Commits first.** The lead commits the settled change under the
  repository's commit rules (Conventional Commits with a lowercase
  imperative subject where the repository states none), with the configured
  Git identity and no model or tool attribution. `ship` delivers those
  commits as they are.
- **One review, beside the checks.** It delegates one independent,
  read-only review of the committed change to a fresh child, preferably on
  a different allowed model, and runs the full build, lint and test suite
  once while the reviewer reads. The reviewer is handed the acceptance
  criteria, the exact base and head commits, the changed paths, the checks
  with their results and the known risks, not the conversation. Its effort
  is the lead's choice: medium for ordinary risk, high where security or an
  invariant is at stake. Only a trivial or mechanical change skips review,
  with the reason on record.
- **Fixes.** Blocking findings are fixed in new commits and the affected
  checks rerun; the full suite runs again only for a fix with broad impact
  or where the project's policy demands it. A second look, by a new child
  that reads the delta and the callers it affects, is owed after a failed
  verdict, and after a pass only when a later commit changes production
  behaviour, security or an invariant, or adds scope. Advisory findings
  are listed and left. Two fix and re-review rounds are allowed before the
  lead must fail visibly; only an unresolved blocking finding, a failed
  check or a review that did not run fails the stage.

That review is the pipeline's: `adaptive` declares
`[pipeline.review] level = "standard"` on `build` (see "A declared
review"), so Archon fails an item whose `build` put no passed or
justified skipped review on record, and `ship` refuses to ship without
the `REVIEW:` line Archon writes. `ship` commits nothing: an uncommitted
change that belongs to the task fails it, and so does a head tree other
than the one the lead reported. `ship` supports both `pr` and `local`.

Select it with `pipeline = "adaptive"` or as a repository's
`default_pipeline`. The `review` knob alone never selects it: without a
named pipeline, `review = standard` picks `default-pr-review`. With
`adaptive` named, leave `review` out or say `standard`. `review = strict`
and `review = none` conflict with it, and a ceiling that demands `strict`
refuses it: use `strict-pr-review` there. Proposal and report knobs work
normally: `proposal = lavish` inserts `propose` before `build`,
`report = with-pr` inserts `report` before `ship`, and `report =
before-pr` also requires operator approval. A requested report revision
returns to `build`.

### `errand`

Use `errand` for a small self-contained job that changes no repository:
research, compare, summarize, check, draft or explain. It always answers in
the stage summary. When a visual answer helps, it also writes one
self-contained HTML page at `.archon/reports/errand/index.html` for Archon
to publish in Lavish; a text-only answer returns `"reports": []`.

It runs in scratch, with or without a repository. An attached repository's
operator checkout is read-only by instruction; commands against its code
run only on an independent throwaway copy inside scratch. No outward side
effects are allowed: pushes, PR or issue comments, merges, messages,
purchases and paid API calls stay with the orchestrator. Use `ask` for a
repository question answered in text, and `quick-pr` or another shipping
pipeline for a repository change.

Published pages and their relative assets normally have durable copies outside
scratch, under `<data>/reports/<report-id>/`. If copying fails, Archon publishes
the original with a warning that it disappears when scratch is collected.
Other files it creates (CSVs, scripts, drafts) are temporary scratch files;
the summary lists their paths. `archon gc` deletes scratch directories
after `worktrees.gc_after` (default seven days) following the last finished
run, once the item also passes gc's age checks. `--older-than` overrides
the age cutoff. With `worktrees.auto_gc` on, passes run at startup and
every 30 minutes. See [workspace retention](ARCHITECTURE.md#132-worktree-gc).

### `team-pr`

`team-pr` builds one change as a team and ships it as one pull request.
Use it when the change spans parts that can be built side by side with
little overlap, such as an API and the UI that calls it. For small or
tightly coupled changes, the split costs more than it saves: use
`default-pr-review`.

1. `plan` (work) writes `.archon/plan.md` and commits it on the run branch:
   the brief split per role, the files each role owns, and the interface
   contract between the roles. The roles fork from that commit.
2. `build` (team, roles `backend` and `frontend`) builds each role's part
   from the plan in its own worktree; Archon merges the branches.
3. `e2e` (test, `all_pass`) sets up and runs the product end to end across
   both parts. Defects go back to `build` as `{{feedback}}`, at most twice.
4. `review` (one reviewer, `all_pass`) reviews the merged change, and
   loops back to `build` the same way, at most twice.
5. `ship` delivers the run branch as for the other shipping pipelines.

A balanced tier suits `plan`, `e2e` and `review`; give the `build` roles a
strong coding model. The roles are fixed in the pipeline file, so a
repository with other parts overrides the whole pipeline by name in
`<repo>/.archon/pipelines/team-pr/`: copy `pipelines/team-pr/` there and
change `roles`, and optionally `role_prompts`:

```toml
[[stage]]
id = "build"
kind = "team"
prompt = "build.md"
roles = ["api", "web", "mobile"]
role_prompts = { mobile = "Own apps/mobile/ only; call the API as the plan specifies." }
timeout_minutes = 150
```

`archon pipelines check` in the repository then lists the override. Keep
`plan.md` naming your roles, since the plan stage splits the brief for
them. A team of more roles than the caps allow is refused at enqueue.
