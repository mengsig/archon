# Changelog

## Unreleased

### Refresh

- A refresh only merges its fresh base, and is reviewed and bounded so. Last
  week refreshes took 60 to 330 minutes each, ~900 stage-minutes in all: the
  refresh copied the owner's whole brief as work to do, ran on the
  pipeline's long timeouts, and owed a fresh whole-change review, so a new
  reviewer re-read the whole feature and the refresh became a fix loop.
  Now its brief asks for the merge, conflict resolution and green checks
  only, with the owner's brief quoted as read-only context and other
  findings left as follow-ups. When the branch head is the commit a passing
  review held, Archon previews the merge with `git merge-tree`: a clean
  merge, or one conflicting only in changelogs, top-level `*.md` or `docs/`
  files, owes no reviewer (that review stands, and the refresh fails if it
  changes code beyond the merge); a code conflict owes one reviewer of the
  resolution, read from the merge's first parent. A branch that moved since
  its review keeps the owner's review level, read whole. A refresh whose branch already holds the base
  commit when it is dispatched ends at once, with no checks or reviewer,
  saying there was nothing to merge. Each refresh stage gets
  `[limits] refresh_stage_timeout_minutes` (default 30) unless the request
  names a timeout.

### Review

- A re-review after a fix reads only what changed, keeping the gate whole.
  Since 1.4.9 every re-review read the whole change again, so a `strict`
  review replaced all three reviewers after each fix (one smoke trial
  delegated nine medium reviewers and ran out of time). The stage contract
  now asks, after a fix, for one new whole-change read at the new head
  and, where the level asks for more passes, delta reads from the head the
  earlier reviews read; a commit after passes needs only deltas. On a
  blocking fail the lead lets the reviewers still reading finish before it
  edits, instead of cancelling them, so one fix answers every finding and
  their passes anchor the deltas. The review gate is tightened to match:
  as many passes as the level asks for must have read the branch head
  itself, whole or as an anchored delta. A pass of an earlier head still
  holds beside them and anchors a delta, but no longer counts toward the
  level, so a strict review is three reviewers of the final head. A delta
  whose `base` is its own head read an empty diff and is refused. The
  reviewer effort floor (#329) is unchanged.

### Fixed

- A stale local base branch no longer fails a "no change needed" item or
  inflates a change (#341). In `pr` mode items fork from the freshly fetched
  `origin/<base>`, but the delivery checks counted from the local
  `refs/heads/<base>`, so a no-change item failed with "holds N commit(s)
  beyond" when the local base was N commits behind. Those checks now count
  from the commit the run forked from, recorded at dispatch, or from a
  newer `origin/<base>` the run took in; the local base only when no fork
  was recorded. The fast check's list of affected files starts at the newer
  of the local and `origin`'s base. After the fetch at dispatch, and at each mirror refresh,
  archond also fast-forwards the registered checkout's local base to
  `origin/<base>` when it is strictly behind, so the orchestrator's mirrors
  follow the fresh base. It never resets, rebases, stashes or forces: a base
  that is ahead or diverged, being rebased or bisected, or checked out with
  a change to a tracked file or an untracked file where the move writes
  stays where it is, and the operator is told why once.

## 1.4.9

Released 2026-10-31.

A patch release on 1.4.8. "Update and restart" no longer hangs the terminal
(#335): archond restarts at once while Aether and its stage agents keep
running. Stage agents get more time and are told their deadline, stop only
the processes they started, and can keep a server up for a later stage;
archond cleans up the `lavish-axi poll` processes and Compose stacks that
items left behind (#321, #323). The mobile web app renders Markdown safely
(#319), the README gains a get-started section driven through the chat
(#322), reviewer effort is a floor and verdicts are read by one tolerant
parser, short ship hashes are accepted, and a stage agent Aether blocked for
inspection is resumed.

Archon 1.4.9 needs **Aether 1.2.8**: it is released with and tested on that
release. Neither the installer nor `archon update` pins an Aether version.
The installer installs the latest Aether release only when it finds no
`aether`, and keeps one it finds. `archon update` runs `aether update
--yes`, which installs the latest release, only on a confirmed update:
after a yes, or with `--yes`; `--check`, `--requirements`, `--no-restart`
and a declined question leave Aether as it is. Check with `aether
--version` and update an older one with `aether update`. Aether 1.2.8
serves protocol 25 (was 24) and still serves 17 to 24; Archon keeps asking
for 24 and accepting down to 17, so neither constant moves. Protocol 25
only adds a `credentials` exhaustion class, which Aether shows a client on
24 as `usage_limit`. The daemon protocol stays 23 and the store schema
stays 18: a 1.4.8 store opens unchanged, and going back to 1.4.8 needs no
copy of the data directory.

**Upgrading from 1.4.8**: the 1.4.8 updater still runs its own restart,
which waits for every running stage agent to finish, up to 30 minutes
(#335). Run `archon update --no-restart` first, which installs 1.4.9
alone, then `archon update --yes`: the new archon restarts the 1.4.8
archond in about 10 seconds, keeping its stage agents running, runs
`aether update` (the latest Aether, 1.2.8 at this release) and renews the
orchestrator. An update from 1.4.8 already waiting can be ended with
Ctrl-C. From 1.4.9 on, `archon update` and its yes restart archond in
seconds, keeping Aether and its stage agents running; the #335 entry under
Fixed says when a busy archond restarts later. A new Aether runs only once
its aetherd restarts. In dedicated mode, when archond runs the new archon
before the update returns, the update asks it to restart Archon's own
aetherd in the background after its stage agents finish, as `--wait` and
`--requeue` allow. An archond that restarts only later is not asked:
whenever that aetherd still runs the old Aether, `archon daemon restart`
restarts it. In shared mode, restart your aetherd yourself.

### Cleanup

- archond runs at most one `lavish-axi poll` per report, and stops it once
  the report settles: its session ended, its item ended, a newer page of
  the same title replaced it, or 24 hours passed since it was shown, even
  while a gate waits on it (answer that in the chat). Reports
  without an item, such as the orchestrator's merged pull request pages,
  were followed forever before, and every restart started their polls again
  (#323, #321).
- archond tears down the Docker Compose stacks an item's worktree started
  when its run ends, before a fresh retry sets its worktree aside, and
  before the worktree is removed; no run starts in that worktree, a retry's
  included, until it is done, and a worktree another live item (a restack
  fix or refresh) still works in is left alone. Only projects whose every container runs from
  inside that worktree, with symlinks resolved, are touched: archond checks
  that again just before it acts, stops and removes only the containers it
  checked, and removes the project's networks once none of its containers
  is left. Volumes are always kept: Docker cannot tell whose a volume is
  when another checkout reuses the project's name. Without Docker it does
  nothing. `[worktrees] docker` names the CLI (#321).
- `archon doctor` lists the Compose stacks left in worktrees of items that
  ended, and the `lavish-axi poll` processes beyond the reports archond
  follows, each with the command that clears them; a project that runs from
  more than one item's worktree gets no `down` command, which would stop
  every one of them (#321).
- Stage agents working in a worktree are told to stop every server and
  Compose stack they started before they finish, unless the agent or brief
  that gave them the task asked to leave it running; then they report what
  still runs and how to stop it (#321).

### Stage time

- Built-in stages that build and test get longer timeouts: `direct`'s
  `implement` 180 minutes (was 60, now as long as `adaptive`'s `build`),
  `e2e-testing`'s `setup` and `swarm`'s `plan` 120 (were 60), and
  `agent-testing`'s `plan`, `code-audit`'s `scope` and `refactor`'s
  `review` 90 (were 45, 45 and 60). An ordinary change with a full check
  and a reviewer no longer runs into a hard stop at an hour. The deadline
  still stops the stage when it passes.
- Every stage agent's contract names its deadline: the time Archon stops it
  and every agent it delegated to, and how many minutes after its start that
  is, with an item's own `stage_timeouts` applied. A `work` or `team` stage
  in a worktree is told that, if its task has it commit, it commits each
  working step as it goes, so a retry builds on it. An
  agent resumed for a fix is told its new deadline with the findings.
  Before, an agent saw its deadline only if it asked Aether.

### Review experiments

- A reviewer's ad-hoc experiment (a mutant, probe or repro) that times out
  is inconclusive, never a finding or a detection on its own, and one that
  compiles gets `timeout 120s` instead of 30s. A timeout counted as
  detected before, so a mutant or repro whose build ran past 30 seconds
  was reported as a hang it never showed.

### Stage processes

- Every stage contract tells the agent to stop only processes it started,
  by the PID it recorded, its process group or its Compose project, and
  never by name or pattern (`pkill`, `killall`, `pgrep -f … | xargs kill`):
  agents run as the operator's user, so such a kill reached the operator's
  archond and aetherd and every stage they ran. A delegating agent passes
  the rule on to its children.
- `e2e-testing`'s setup writes a `stop.sh` that stops what `start.sh`
  recorded and nothing found by name, and setup and e2e confirm that none
  of those recorded processes or containers still runs, not that nothing
  runs at all. `agent-testing`'s plan records stop commands the same way.
- A stage can keep a server up for a later stage of its run by starting it
  under `setsid` and writing its PID to `.archon/services/<name>.pid` in the
  worktree it works in, a team role's own included. The stage-end reap
  spares that process, the process group it leads and its descendants while
  the run goes on (not a group it only joined, which `cmd &` shares with
  the engine's every stray), and the run's end reaps what still runs from
  the worktree. Before, the reap stopped every process a stage left, so
  setup could not hand a running app to the next stage. A malformed PID
  file keeps nothing. The PID files do not count as a change when the run
  ships, and a team role's are left out of the commit Archon makes of its
  uncommitted work, so two roles can declare the same name.

### Mobile web app

- Markdown renders in the mobile web app: chat bubbles and feed lines,
  stage summaries and errors, question text, notifications and item
  briefs show headings, emphasis, code, lists, task lists, quotes, tables
  and links instead of raw `**` and `#`. The renderer has no dependencies
  and builds every node as text, so raw HTML shows as written, links allow
  only `http`, `https` and `mailto` (opened with `noopener noreferrer`),
  and images become links that never load (#319, by @AmmarBattah in #327).

### Agents

- A ship record whose `commit` or `tree` is an abbreviated hash, as
  `git commit` prints one, is resolved in the run worktree and stored in
  full instead of failing the run; only a prefix that names no single
  object of that type is refused, and the refusal says so.
- The automatic repair asks only for what was refused: a refused ship
  record alone gets a note to hand in a new record with full hashes and
  change nothing, and a reviewer only when the stage owes one. A delivery
  refused for what the record stage can fix in its worktree (uncommitted
  changes, a tree that is not the record's, files that must not ship, a
  foreign commit identity or an attribution trailer) is now repaired too; a foreign identity is still
  refused until the commits carry the repository's own.
- An infrastructure failure wakes the failed agent in place, with a note
  that names the failure and its new deadline, when Aether still has it; it keeps its worktree
  and history. A fresh agent starts only when Aether refuses the wake, and
  a wake that took does not use up the stage's infrastructure retry. A
  failure that names a passing cause (an unavailable engine, a rate or
  usage limit, a refused login) is retried after 5 minutes, with the
  deadline moved by the wait.
- A stage whose agent Aether holds `blocked` (a usage limit, credentials)
  or waiting on an approval is no longer timed out: its deadline is moved
  on while it is held.
- A stage agent that Aether `blocked` for inspection no longer sits idle
  until its deadline. Aether does this when a command's outcome was lost,
  as when an older aetherd restarted mid-command. Once the stage has been
  silent for `limits.stall_minutes`, with no gate or question open, the
  agent gets a note to check the state before it repeats the command, and
  is resumed. If Aether refuses the resume and the agent is still blocked,
  the stage fails as infrastructure at once, so its infrastructure retry
  wakes or replaces the agent. While the watchdog is on, an inspection holds the deadline like
  any other hold.
- Some holds have no cause Aether names: not an approval, a usage limit or
  credentials. When such a hold has been silent for
  `limits.stall_fallback_minutes`, with no gate or question open, the
  orchestrator is told once. The stage is never stopped for it.
- A Claude Code stage under a subscription login no longer gets Aether's
  hard dollar cap. Archon detects the login from the 5-hour and weekly
  usage windows Aether reports for it, when they are fresh and have headroom
  left. The dollars it would be stopped on are list-price, not billed. Its interim usage is not refused as
  unaccounted either. Its cost is still recorded and shown, and Archon's
  ledger budgets still count it. When Archon cannot tell whether a Claude
  Code login bills, the cap stays, and a stop on it is labelled "notional
  cost".
- A kept agent resumed for a later attempt, or a failed one woken, gets an
  Aether cost cap of what it was charged so far plus its new grant. The
  cap read that spend from stage costs Archon never stored, so it was the
  grant alone and could stop the agent soon after it went on.

### Fixed

- "Update and restart" no longer hangs the terminal (#335). After a yes, at
  launch or with `archon update --yes`, archond restarts at once while Aether
  and its stage agents keep running (the `--keep-aether` restart), and the
  new archond takes up their stages; the update then renews the
  orchestrator and opens what you ran against the new archond, in seconds.
  Before, the restart waited for every running stage agent to finish, up to
  30 minutes, and 1.4.7 also ignored Ctrl-C there. A restart that keeps
  Aether now waits only for archond's own spawns, merges and deliveries; the
  update waits 30 seconds for it at most, and an archond still busy then
  restarts by itself once they end, or gives up at `--wait`, which the
  update says before it returns. An archond older than 1.4.9 still waits
  for its agents, so the update gives it 10 seconds and has it restart
  anyway, keeping them running. A new Aether
  restarts Archon's own aetherd in the background, after its stage agents
  finish, when archond runs the new archon before the update returns;
  otherwise `archon daemon restart` does it. Ctrl-C at launch exits instead of opening the old archon. The
  question no longer says it waits for running agents, and the install
  report names no commands to type. The update from 1.4.8 still runs 1.4.8's
  own restart: to skip its wait, run `archon update --no-restart`, then
  `archon update --yes`, which restarts the 1.4.8 archond in about 10
  seconds. `scripts/simulate-update.sh` runs both updates end to end in a
  sandbox with a stage agent running.
- Stage gates and delegated reviews read verdicts with one tolerant
  parser: a structured `verdict` key, whatever it holds, then a structured
  `result` or `status` that opens with a verdict word or a `VERDICT:`
  label (an unreadable one counting as fail), then the summary's last
  `VERDICT:` line at column 0 outside block
  quotes (and the lines that continue one) and fences, in any letter case and
  through markdown emphasis (`**Verdict:** PASS`), with `passed`, `approved`,
  `failed` and `rejected` read as pass and fail. A test stage that passed
  with `{"findings": []}` and `**Verdict:** PASS` in its summary no longer
  fails its gate, and a reviewer whose `structured` gives no verdict is read
  from its summary, its `head:` and `base:` lines only at the top level;
  a review whose `structured`, summary JSON or `head:`/`base:` lines name
  different commits is refused, a structured verdict included. A summary's JSON verdict counts only in a column-0
  fenced block, never an example nested in a list, indented or quoted. An unreadable verdict, a choice or hedge such as
  `PASS or FAIL`, `PASS, but two failed` or `PASS (pending fixes)` included
  (any word between the verdict and a `—`, `:` or `.`), or a pass
  explained in anything but plain success words and numbers
  (`PASS — all 214 tests pass` counts; `PASS — merge only with green CI`
  does not), or followed in the summary by any line but `head:` and `base:`,
  still counts as fail,
  and the refusal now quotes it. A structured pass beside a summary
  whose final `VERDICT:` line or JSON verdict (`verdict`, `result` or
  `status`) does not pass counts as fail too, and so does any pass beside
  a finding of `blocking`, `blocker`, `critical` or `high` severity in
  `structured` or the summary's JSON; a summary written as the
  `{summary, structured}` arguments has its inner summary read too.
- A reviewer plan's effort is a floor, not a ceiling. The delegation
  watchdog cancelled a reviewer delegated above the planned effort in the
  middle of its review, and let one below it run, which is how
  thirty-second low-effort reviews passed. A reviewer at or above the plan,
  up to the model policy's `max_effort`, now runs; one below it is
  cancelled as it spawns, before it does work, with a message naming the
  floor, and the contract states the floor so the lead delegates at it.
  Reviewers no plan names are asked for at least medium effort, and at
  least high when the lead rates the change's risk medium or high, whatever
  `[review] default_effort` says, within the policy's cap. A re-review after
  a fix now reads the whole change from its base, in the contract and in
  the `adaptive`, `swarm` and `direct` prompts; Archon still counts delta
  reads.

### Documentation

- A "Get started in the chat" section in the README and a
  [getting started](docs/getting-started.md) walk-through: registering a
  repository, its settings and `/answer` confirmations, queueing, watching
  and merging work, standing directives, Lavish review pages, `/watch`,
  `/questions` and `/lessons`, all from the orchestrator's chat (#322).

### Tests

- Three tests that failed under load are de-flaked: the slow-start wait
  test logs steadily for 3 s against a 2 s silence limit, the full-check
  limit test gives its command 3 s, and three fake `gh` scripts are written
  so that a fork on another test thread cannot hold them open (#332).

## 1.4.8

Released 2026-10-29.

A patch release on 1.4.7 that brings the mobile web app with `archon remote
setup` (#312), and the companion fixes of the end-to-end sweep: bounded
storage with history retention, restacks over squash- and rebase-landed
dependencies, message caps that keep a stage audible, steer regrants that
reach delegated helpers, event pump recovery from expired history, an
engine login check in the doctor, and an `archon update --yes` that
restarts the orchestrator.

Archon 1.4.8 needs **Aether 1.2.7**: it is released with and tested on that
release, which the installer and `archon update` install. The Aether
protocol Archon asks for stays 24 and the oldest it accepts stays 17. With
Aether 1.2.7 it also uses two opt-in features: `subtree_update`, which
raises a steer's deadline or budget for a stage agent and its helpers in
one request, and `history_expired`, which reports expired event history by
its own code. An older aetherd serving protocol 24 still works, with one
update per helper and expired history read from the refusal's text.

The daemon protocol is 23 (was 22), for the remote-access requests:
`archon mcp` and archond must be upgraded together, and each refuses the
other's older version at hello. The store schema is 18 (was 17): the first
start upgrades a 1.4.7 store in place, so learning outlives the history
retention deletes, and adds the total of pruned spend. Archon 1.4.7 refuses a
schema 18 store, so going back needs a copy of the data directory from
before the upgrade.

**Upgrading from 1.4.7** with `archon update --yes`: the 1.4.7 updater
cannot speak protocol 23, so after the drained restart it waits out its
deadline (`--wait` plus five minutes), reports that no new archond answered
and leaves the orchestrator on 1.4.7. archond has restarted on the new
release; run `archon update --yes` again, now from the new release, to
refresh Archon's tools and renew the orchestrator. Later updates do this
themselves (see Update below).

**After upgrading**, run `archon update` and accept the restart (`--yes`
for scripts): it updates Aether to 1.2.7 and restarts both daemons, archond
and, in dedicated mode, Archon's own aetherd, then the orchestrator.
Optionally, run `archon remote setup` to switch on the mobile web app and
pair your phone ([docs/remote-setup.md](docs/remote-setup.md)).

### Mobile web app

- archond can serve a mobile web app ([docs/remote.md](docs/remote.md)):
  the queue with filters by state, repository, pipeline, priority and
  blockers; each item's stages, verdicts, summaries, cost, pull request,
  artifacts and reports; the operator chat, live; the questions and gates
  waiting on you, answered in place; enqueue, cancel, retry, pause and
  resume; and an overview of agents against caps, budgets, spend,
  repositories and pipelines. Light and dark, and it honours reduced
  motion (#312).
- It is off until you switch it on, binds loopback only (`[remote]` in
  `config.toml`), and is published over HTTPS by `tailscale serve` or your
  own proxy. A device pairs once with a one-time code (or its QR code) and
  gets its own credential, stored only as a hash and revoked at once with
  `archon remote revoke`. Each request a device sends is checked against a
  fixed list and handled as the operator CLI's; merges, promotions,
  settings and ceilings stay on the CLI (#312).
- `archon remote setup` switches it on, publishes it on your tailnet,
  checks the address and pairs your phone, explaining each step; it is
  safe to run again and takes `--yes`, `--json`, `--proxy-url`, `--local`,
  `--https-port` and `--no-pair` for scripts. `archon remote` also has
  `status`, `enable`, `disable`, `pair`, `devices`, `revoke` and `origin`.
  The installer (`ARCHON_REMOTE=1`) and the first run of `archon` offer it
  as an optional step ([docs/remote-setup.md](docs/remote-setup.md), #312).
- `make e2e` runs the app against a real archond in a headless browser:
  pairing, the queue and an item, the chat, answering a gate, enqueue,
  cancel, pause and resume, and a revoke and a disable from the CLI. CI
  runs it on the release pull request into `main` (#312).
- A failed pipeline listing in the new-item sheet and a failed page read
  in an artifact's sheet show with "Try again", as the other loads do.
- When archond closes a session (a revoke, a disable or a withdrawn
  origin), it reads until the device answers the close. Before, a frame
  it had not yet read could reset the connection, so a revoked phone saw
  a dropped connection and kept retrying instead of showing it is no
  longer paired.
- An open item no longer redraws once more when the repositories load
  after its detail, which dropped a half-read brief or a focused button
  (#316).

### Agents and messages

- A stage agent at `[limits] max_agent_messages` keeps three escalations
  and one reply to each orchestrator message, and the orchestrator is told
  once that it reached the cap (#306, #299).
- `steer --extend` and budget regrants reach the helpers a stage lead
  delegated to, not only the lead, in one request on an Aether 1.2.7 that
  serves `subtree_update` (#302, #273).
- `model_plan` choices by `tier` and `provider` are accepted by the MCP
  tools and in initiative members (#296, #300, #297).
- The event pump recovers when Aether's retention deleted events past its
  cursor; a stage whose agent finished but whose settle failed keeps the
  agent's result instead of timing out; and a retried trigger batch reaches
  a replaced orchestrator (#305, #272).

### Storage

- `archon gc` also has Aether delete the workspaces it made for the agents
  a stage delegated to, then runs `git worktree prune`; a data directory
  with mirrors in it can be removed with `rm -rf` (#308, #281).
- archond deletes the history of items settled more than `[storage]
  retention_days` ago (14 by default, `0` keeps everything), keeping
  learning, the total spend and any item still on disk or still counted.
  The SQLite WAL is cut back to 4 MiB, and the data directory is made
  readable by you alone (`0700`, database files `0600`), an existing one
  included (#308).

### Stacks

- The dependents of a dependency merged on GitHub as a squash or a rebase
  are restacked onto the landed commits instead of meeting a conflicting
  merge, and `refresh_item` no longer refuses such a branch as up to date
  (#309, #304).

### Update

- `archon update --yes` restarts the orchestrator, ends at a signal at any
  step and refuses a partial environment (#307, #271).
- When archond restarts on a release with another daemon protocol, `archon
  update --yes` takes the new archond's answer at hello as the restart done,
  instead of waiting out its deadline, and renews the orchestrator through
  the installed archon (#314).

### Doctor

- `archon doctor` and setup check that the orchestrator's engine is logged
  in and name the login step, the revival blocker names it too, and `archon
  tui` stops retrying a failed orchestrator; the release check no longer
  needs `gh` (#310, #277).

### Fixed

- A stage agent no longer inherits `ARCHON_DAEMON_DETACHED` from archond: the
  detached start now tells its daemon with a hidden `--detached-child` flag,
  so `archon daemon run` run by an agent's tests logs to the terminal as it
  does in a plain shell (#315).

### Docs

- Docs and CLI help now name `codex/gpt-6.1-terra` as the example Codex model,
  following Aether making GPT-6.1 Terra the Codex default (supersedes #303).

## 1.4.7

Released 2026-10-27.

A patch release on 1.4.6 with the fixes from the end-to-end sweep (#270):
an agent's shell no longer acts as the operator, reviewers diff against the
fork point instead of a stale local base, a reviewed `ship = local` item can
be refreshed and then merged, retries of keyed requests are idempotent, and
lessons are no longer retracted on cancels, budget caps and timeouts.

The store schema is unchanged (17), so Archon 1.4.6 still opens a 1.4.7
store. The daemon protocol is 22 (was 21), as `update_item` gained `title`:
`archon mcp` and archond must be upgraded together, and each refuses the
other's older version at hello. The Aether protocol Archon asks for stays 24;
the oldest it accepts is now 17 (was 14), Aether's own floor, so an aetherd
serving only 15 or 16 is refused at connect.

Archon 1.4.7 needs **Aether 1.2.5**, the release it is tested on and that the
installer and `archon update` install. No change in this release needs a
newer Aether. Archon checks no Aether version, so an older aetherd that
serves protocol 17 or later is not refused, but only Aether 1.2.5 is
supported with this release.

**After upgrading**, run `archon update`. It installs Aether 1.2.5 and
restarts both daemons, archond and Archon's own aetherd, through the drained
restart.

### Authority

- The `archon` CLI run from an engine's shell acts for the agent Aether's
  `AETHER_ENGINE_AGENT` and `AETHER_ENGINE_RUN` name, not as the operator:
  archond refuses it operator-only requests and lets only the orchestrator
  change Archon. Reads keep working, and an engine child that names no agent
  sends only reads (#291, [#276](https://github.com/mengsig/archon/issues/276)).
- A gate answer is refused when the operator's message negates the answer
  word ("don't approve the migration gate"), even if the cited quote leaves
  the negation out (#291).
- The orchestrator and stages no longer see Aether's native `notify`, which
  bypassed Archon's feed, push and notification center; stages also lose
  `refresh_provider_usage`. Archon's own `notify` stays (#291).

### Ship checks

- The clean check before delivery accepts a smudged LFS file whose content
  matches the committed pointer (its bytes, or its size and SHA-256), so an
  LFS repository's checkout is no longer refused as dirty. It runs no LFS
  filter; a mismatch, deletion or mode change stays dirty (#292).

### Prompts and pipelines

- Every built-in prompt, the shared proposal and report, and the stage
  contract's dependency rules diff and log against the new run variable
  `base_commit`, the branch's fork point from the local and fetched base,
  instead of the operator's local base branch, which could be far behind
  and made reviewers read upstream commits as the item's change. A refresh
  reviews from the base commit it merges. Runs saved before it fall back to
  the base's name (#284, [#275](https://github.com/mengsig/archon/issues/275)).
- code-audit: the scope lead writes a numbered `lenses.md` in which the
  brief's own lenses win over the defaults. swarm: a task may consume a path
  the fork commit already holds, and stage listings number each rerun of a
  slot (#284).

### Local shipping

- `archon merge` of a reviewed `ship = local` item lands the commit its
  newest succeeded refresh or restack fix recorded, instead of refusing a
  refreshed branch as "moved since it was reviewed". A grant still requires
  every delivery behind the landed commit to have had a review
  (#285, [#274](https://github.com/mengsig/archon/issues/274)).
- `archon queue add --onto` works in a `ship = local` repository: the new
  item works on the target item's branch, in its worktree, and ships as a
  local commit there (#285).
- A retry of an item whose proposal was approved, on the same branch and
  brief, starts past the proposal gate instead of asking again (#285).
- `update_item` (and the MCP tool) takes `title` until the item's first run
  is dispatched (#285).
- A stage that reports no change is no longer failed by Archon's own report
  pages in the worktree (#285).

### Queue and lessons

- A keyed `enqueue` retried with the same arguments is no longer refused as
  a different request when routing filled another model plan: the
  fingerprint holds the plan as asked. A key recorded by 1.4.6 holds the
  plan filled then, so it matches only a retry that fills the plan alike
  (#287, [#279](https://github.com/mengsig/archon/issues/279)).
- `queue wait` reads every item before it exits 5 and names each item
  blocked on a failed dependency, instead of stopping at the first (#287).
- `settings --set` and `--reset` start archond like other changes; a plain
  `settings` read still does not (#287).
- Lesson scoring ignores runs that judged nothing (a cancel, a budget cap, a
  timeout or an infrastructure failure), so they neither count against a
  lesson nor retract it, including the uses of such runs that 1.4.6 already
  stored as losses; `lessons stats` takes its pass rate over scored
  runs and shows how many. An activation or retraction by scoring posts a
  feed line (#288, [#280](https://github.com/mengsig/archon/issues/280)).

### Chat and TUI

- `/image` in line chat sends the attachment even where the editor cannot
  show the typed-ahead chip, a failed send keeps the images and the text,
  and leaving the chat names images never sent
  (#286, [#278](https://github.com/mengsig/archon/issues/278)).
- The TUI's `/status`, `/queue`, `/item`, `/questions`, `/repos` and
  `/lessons` are answered by archond in a note, without an orchestrator
  turn (#286).

### Notifications and CLI wording

- Cancelling items that never started posts no notification each, so a
  bulk cancel no longer floods the center; `/merge` of work already landed
  is an outcome, not a blocker
  (#289, [#282](https://github.com/mengsig/archon/issues/282)).
- Status says whether held news is due or rides along with the next
  trigger, and sums storage on one line (`--verbose` lists each directory).
  The budget blocker names the item and shows spend, cap and overrun;
  settings show budgets and caps as status does; cost names the
  orchestrator's row; model-evidence names repositories; and several
  messages read correctly ("1 minute", no stray `**` or "..") (#289).

### Docs, link workers and CI

- `docs/troubleshooting.md` has an entry for each blocker archond posts,
  with the next command, and a test fails when a new blocker is not
  documented. The guide and the orchestrator prompt say the orchestrator
  can schedule recurring work
  (#290, [#283](https://github.com/mengsig/archon/issues/283)).
- The link tests run the same supervised link workers archond runs, and a
  failed worker is restarted (#290).
- CONTRIBUTING and `ci.yml` explain why pull requests into `next` run lint
  only (#290).
- Dependencies: the GitHub Actions group, tokio 1.54.1 and rustyline 19.0.0
  (#269).

## 1.4.6

Released 2026-10-25.

A patch release on 1.4.5 for long-running fleets: stalled stages are
retried and then failed instead of waiting a day, processes a stage leaves
behind are reaped, gc no longer removes a worktree a live item uses, and
changes made through MCP can be repeated safely and followed after a
timeout.

Store migration 16 adds the `tracked_requests` table, and migration 17
indexes the refreshes 1.4.5 replaced (see Worktrees); Archon 1.4.5 refuses
the migrated store. The daemon protocol is 21 (was 19), as `refresh_item`
gained fields and `request_status` is new: `archon mcp` and archond must be
upgraded together, and each refuses the other's older version at hello. The
Aether protocol Archon asks for stays 24.

Archon 1.4.6 needs **Aether 1.2.5**, the release it is tested on and that the
installer and `archon update` install. The stage watchdog reads its
`engine_alive` and `last_event_at` agent fields, and it reaps an agent's
leftover processes itself. Archon checks no Aether version, so an older
aetherd is not refused, but only Aether 1.2.5 is supported with this release.

**After upgrading**, run `archon update`. It installs Aether 1.2.5 and
restarts both daemons, archond and Archon's own aetherd, through the drained
restart. Aether 1.2.5 migrates its own store to schema 27 on that start.

### Stage watchdog

- A stage whose agents Aether left `running` with no live engine and no
  event for `limits.stall_minutes` (default 20) is stalled; on an Aether
  that does not report `engine_alive`, silence alone over
  `limits.stall_fallback_minutes` (default 45) is. An open gate or
  question, an approval wait, a `blocked` agent or a live engine never
  stalls. A stalled stage settles as an infrastructure failure ("agent
  stalled: no engine and no events for N min"), so `limits.infra_retries`
  runs it once more on the same worktree and a second stall is final. Before,
  such a stage waited for its stage timeout, a day by default
  (#257, [#251](https://github.com/mengsig/archon/issues/251)).
- `stall_minutes = 0` turns the watchdog off.

### Process reaping (Linux)

- When a stage settles and no other stage of its run is live, archond ends
  the processes still running in the item's worktree (SIGTERM, then
  SIGKILL), such as a model server or a `nohup` job that escaped the
  engine's process group. What was ended or survived is added to the stage
  summary and the feed. Item cleanup and `archon gc` do the same before git
  removes a worktree, and keep the worktree when a process survives
  (#258, [#254](https://github.com/mengsig/archon/issues/254)).
- Only processes of the same user that an Aether engine started (they carry
  `AETHER_ENGINE_RUN` in their environment) are reaped; an operator's shell
  or editor in a worktree is never touched, nor pid 1, the archon and aether
  daemons, their ancestors or their descendants.

### MCP tools

- `refresh_item`, `retry_item`, `steer_item`, `update_item`, `cancel_item`,
  `notify`, `github_reply`, `request_merge` and `message_orchestrator` take
  an optional `idempotency_key`. For 24 h a repeat with the same key and
  arguments returns the first result and changes nothing; the key with other
  arguments is refused. Keys are kept in the store, so they hold across a
  restart (#259, [#252](https://github.com/mengsig/archon/issues/252)).
- A change archond does not answer within the 15 s budget now returns
  `pending: req_…` instead of a timeout error, and the new read-only
  `request_status` tool reports whether it was applied, failed, is still
  pending, or was interrupted by a restart, with its result. It is answered
  while archond is in standby too. When archond cannot record a change's
  outcome, the call fails and says whether the change itself was applied.
  `request_status` reports the outcome from memory until a later prune pass
  records it. While 128 such requests are held, running or unrecorded, a new
  change is refused before it runs (#259).
- `refresh_item` takes optional `model_plan`, `model_reasons` and
  `stage_timeouts`, checked as in `enqueue`. Without them a refresh runs on
  the stage defaults instead of copying the finished item's plan
  (#261, [#255](https://github.com/mengsig/archon/issues/255)).

### Orchestration

- Pull request checks GitHub never started (billing, no runner) are a quiet
  `pr_checks` feed line first, and wake the orchestrator once only if they
  are still unstarted 30 minutes later; under `checks = "waived"` they stay
  a quiet line. A pull request with no checks is a quiet line too (#261).
- A delegated agent's effort shows in `get_item`'s cost split while its
  stage still runs, instead of "effort unknown" (#261).
- An agent of a running stage that leaves an inbox item unread past
  `[dispatcher] inbox_unread_after` (15 minutes by default) shows as a stage
  health warning in `get_item` and `status`, the sign of a lead that died
  while its children finished (#261).

### Daemon

- The read tools the orchestrator polls (`list_queue`, `get_item`,
  `get_stage`, `list_repos`, `queue_status`) read on read-only connections
  of their own, so they no longer wait behind a write: with a write held for 3 s, `list_queue`
  answered in 6 ms instead of 3 s. Each read holds one snapshot of the
  store, so its parts agree with each other. WAL checkpoints run every 2 s on
  their own connection, off the writer's lock (#259).

### Worktrees

- gc: a worktree is kept while any non-terminal item uses it, not only
  while the item whose run left it is live. A refresh or restack fix works
  in its owner's worktree, so a pass that collected an earlier, settled
  refresh of an item removed the worktree a later refresh of it was
  running in. A pass now decides by path: it ages a worktree by the latest
  run any of its items had there, leaves out every place a queued,
  dispatching, running or blocked item or a live run uses, and marks the
  worktree's owner too, so dispatch holds a refresh queued meanwhile until
  the pass is done. `archond`'s removal after shipping asks the same. A
  refresh that replaced an earlier one for the same base commit still
  counts that one's runs, so retrying it does not let gc remove the
  worktree it just used. Store migration 17 does the same for a refresh
  that 1.4.5 replaced (#260, [#256](https://github.com/mengsig/archon/issues/256)).
- A stage whose workspace is gone fails with `workspace missing: <path>`,
  its engine's error after it, instead of only that error (such as an engine
  "not installed"), and is not retried as an infrastructure failure there.
  A retry adds the worktree again from the item's kept branch (#260).

## 1.4.5

Released 2026-10-24.

A patch release on 1.4.4, with no store migration and the same daemon
protocol (19). The stack parent's new `closed` mark is a defaulted field in
the stack row's JSON, so older rows read as before.

Archon 1.4.5 needs **Aether 1.2.4**, the release it is tested on and that the
installer and `archon update` install. Archon still asks Aether for protocol
24 and checks no Aether version, so an older aetherd is not refused, but only
Aether 1.2.4 is supported with this release.

**After upgrading**, run `archon update`. It installs Aether 1.2.4 and
restarts both daemons, archond and Archon's own aetherd, through the drained
restart.

### Stacks

- restack: a stack parent whose pull request closed without merging no
  longer pins its child. Before, it stayed "unmerged" forever, so every
  restack pass moved the child's pull request back onto the dead branch with
  `gh pr edit --base`, undoing a manual retarget, and merged that branch into
  the child again. The parent is now marked closed when its pull request
  settles unmerged; the base, the fork, restack merges, the delivery proof,
  the operator merge and refresh skip it, so the child falls back to the next
  open parent or the repository base and stays there. A reopened pull
  request does not make the parent open again (#244).

### Agents tree

- Stage agents under a run node are named by their stage alone
  (`implement`, `review 2/3`, `ship`), with attempt and retry notes only
  above one, because the run node's row already names the item and its
  title. Agents created before the upgrade keep their names. Aether 1.2.4
  shows such a passive run node as a group header with the subtree's status
  and cost (#247, #248).

## 1.4.4

Released 2026-10-23.

A hotfix release on 1.4.3, with no store migration and the same daemon
protocol (19). Archon 1.4.4 still needs **Aether 1.2.3**. Anyone who removes
repositories should upgrade: `archon update` restarts archond on the fix.

- daemon: archond no longer hangs when it watches the checks of a delivered
  pull request whose repository was since removed (`archon repo remove`).
  Ending that watch took the store's lock while the lookup that found the
  repository missing still held it, so archond deadlocked at its next
  maintenance pass: every `archon` command, the TUI and the `archon` MCP
  tools waited forever, and a restart hung again within minutes because the
  watch was never deleted. The watch now ends quietly, as intended. An
  archond already hung does not answer `archon update`: stop it with
  `kill $(cat $XDG_RUNTIME_DIR/archon/archond.pid)`, install 1.4.4 and start
  it, and it clears the watch. Agents running in Aether are picked up again.

## 1.4.3

Released 2026-10-22.

A patch release on 1.4.2, with no store migration. The daemon protocol is now
19 (`archon gc --preview`, #234; a 1.4.2 archond would ignore the preview and
collect for real). archond and `archon` must speak the same protocol, so a
1.4.2 archond refuses a 1.4.3 client until it is restarted.

Archon 1.4.3 needs **Aether 1.2.3**, the release it is tested on and that the
installer and `archon update` install. Archon still asks Aether for protocol
24 and checks no Aether version, so an older aetherd is not refused, but only
Aether 1.2.3 is supported with this release.

**After upgrading**, run `archon update`. It installs Aether 1.2.3 and
restarts both daemons, archond and Archon's own aetherd, through the drained
restart, so the new protocol, settings and watchers take effect.

New settings, all in `config.toml`:

- `[limits] max_load_per_cpu` and `[limits] min_available_memory_gb`, both
  off unless you set them (#72).
- `[review] auto_repair`, on by default (#237).

### Review gate

- A reviewer's result is also read from one fenced JSON block or a final
  `VERDICT: PASS|FAIL` line with `head:`/`base:` lines; prose without a
  verdict is refused with what to send instead, and a fenced verdict that
  contradicts the final line is refused. A reviewer id the lead mistyped is
  resolved to the one lead child it resembles, else to the lead's completed
  reviewers with a verdict, and coverage is checked as before; a refusal
  lists the lead's reviewer ids. The stage record keeps each reviewer's
  verdict, head, base and finding count, shown by `archon show` (#237).
- When a lead's review-fix loop has had four commits failed by a reviewer,
  archond posts one blocker notification with the item, the round count and
  the latest finding titles, and the run goes on. Three parallel reviewers
  of one commit are one round; the count restarts with each stage attempt
  (#237).
- A run that fails only because the gate refused its review or ship record
  is retried once, automatically, in the same worktree and branch, with a
  note that quotes the refusal and asks for the record to be repaired. The
  feed says so and the failed run stays in the item's history; a second
  refusal fails the item with both reasons. Cancelled and steered runs and
  every other failure are untouched. `[review] auto_repair = false` turns it
  off (#237).
- Under an item budget, a reviewer that cannot report its cost is refused
  when the item is queued, and a combined autonomy and budget update is
  checked against the review it leaves (#191).

### Merge, stacks and queue

- A merge request no longer fails while GitHub is still computing whether
  the pull request can merge. It waits, reading again with a 5-60 s pause
  for up to 60 minutes from when the wait began; it merges once GitHub says
  it can, refuses on a conflict, a moved head or the timeout, and never asks
  the operator again for the same pull request at the same head (#241).
- A stacked item can be merged and diffed after its parent merged; a pull
  request still aimed at the merged parent is retargeted onto the base, and
  the not-merged error names the parent to merge first. Steer on a cancelled
  item is refused and points at retry (#225).
- Restack first fast-forwards the branch to its pull request's head, so
  commits made outside Archon are kept, and records parents the branch
  already contains. A push the remote refuses as not a fast-forward is told
  to the operator once per pull request head and not retried until the
  parent moves, instead of looping (#203).

### Host resources and gc

- Dispatch holds new work while the host is too busy: past
  `[limits] max_load_per_cpu` (1-minute load per CPU) or below
  `[limits] min_available_memory_gb` (`MemAvailable`), no item and no stage
  agent starts until load is below 90% of its limit and memory 10% above;
  running agents finish. `archon status` and each waiting item show the
  measured values and the next step. Both are off by default (#72).
- `archon gc --preview` lists the worktrees gc would remove and the build
  output it would delete, and deletes nothing. With `[worktrees] cache_dirs`
  unset, gc now deletes only known build output (`target`, `node_modules`,
  `.venv`, `__pycache__`, `dist` and similar) and keeps other ignored files
  such as `.env` (#72, #226).
- Archon's own `.archon/reports/` pages and `.archon/swarm-plan.json` no
  longer make a worktree count as dirty, so gc removes it as it should;
  `.archon/config.toml` and pipelines still count (#226).
- The "mixed locations" guard no longer blocks archond from starting (after
  a crash, for instance) when private XDG config and data directories sit
  beside an `ARCHON_SOCKET`; `archon doctor` uses the same check (#226).

### Orchestrator and feed

- archond tells the operator, and nudges the orchestrator to continue, when
  it has had a turn open for 30 minutes with no message, tool result or
  usage update (a hung model request). At most three nudges per
  orchestrator; an archond restart into an open turn still watches it
  (#223).
- A batch of operator messages queued while the orchestrator was busy gets
  one reply that names them all, so none is left without an answer; the log
  labels the orchestrator by its `--kind` name (#227).

### Pipelines

- Swarm: the planner's `.archon/swarm-plan.json` stays an uncommitted file
  in the run worktree, hidden through `info/exclude`, so no shipped history
  carries it (archond refuses a plan the planner committed or staged, or
  that appears anywhere in the history). Archon deletes each role's local
  branch once its merge is recorded and again before the run is marked
  finished. Role branches are never pushed (#195).

### CLI and docs

- A malformed item id reads "'x' is not a valid item id" with the expected
  format and `archon queue list` to find ids. The autonomy ceiling refusal
  and `archon status` say which values the ceiling allows and how to raise
  it. `archon queue add --help` and the docs show the `provider/model`
  model plan format (#228).

## 1.4.2

Released 2026-10-22.

A patch release on 1.4.1, with no store migration. The daemon protocol stays
18, but `archon merge` and `archon diff` use requests a 1.4.1 archond does not
know, so restart archond after upgrading (`archon update` does).

Archon 1.4.2 is released with **Aether 1.2.2**, which the installer and
`archon update` install. Archon still asks Aether for protocol 24, so an older
aetherd that serves it runs everything as before; the `archon tui` welcome
block needs Aether 1.2.2, and image paste and notifications need 1.2.1 or
newer, as in 1.4.1. Archon starts Aether with `AETHER_MANAGED_BY=archon`, so
Aether 1.2.2's own update offer at launch stays off there and `archon update`
updates both.

**After upgrading**, run `archon clear-notifications --all` once. Older
versions left blocker and outcome notifications open that nothing resolves
now; new ones resolve themselves.

### Update and first run

- `archon update` is one command: after it installs the release it asks
  (`--yes` answers, `--no-restart` declines), updates Aether, restarts
  archond and Archon's own aetherd through the drained restart, and
  restarts the orchestrator. Your own aetherd is never restarted
  ([docs/install.md](docs/install.md#updating), #188).
- Every `archon`, `archon tui` and `archon chat` on a terminal checks for a
  newer Archon or Aether and offers the update, with a per-version skip,
  `[update] check_on_launch = false` and `ARCHON_NO_UPDATE_CHECK=1` to opt
  out. It never waits more than 1.5 s, stays silent offline and caches its
  answer for `[update] cache_for`. Reminder idea from #187 by @Markfds01.
- Update suggestions (#208): `archon status` ends with `run archon update`
  when a checked latest release is newer; the launch prompt links the
  release notes and says that running agents are waited for and what
  restarts; `/update` (terminal UI menu, `archon chat`, Aether chat) is
  `/upgrade both latest`; a declined first-run offer to register a checkout
  is remembered per directory and not asked again.
- A bare `archon` on a new machine offers the first-run setup, `--help`
  groups its commands, and the README's quick start is one command (#205).
- `archon tui` shows an Archon welcome block (repositories, queue counts, an
  example request) with Aether 1.2.2, and gains `/repos` and `/lessons`
  (#210).

### Local ship

- A ship = local result can be reviewed and landed: `archon diff` and `/diff`
  show its branch, repository and diffstat, and `archon merge` fast-forwards
  the base to the reviewed commit through the same confirmation as a pull
  request. It refuses, changing nothing, when the base moved, the history is
  unrelated or the checked-out base has local changes, and never forces
  (#206).

### Notifications and attention

- Notification center: an item's blockers (such as a stage's escalation) are
  resolved when the item is steered, one of its questions is answered, or it
  finishes, so "waiting" no longer counts what was acted on. A succeeded item
  whose summary says `BLOCKED:` is an outcome, not a blocker. New
  `archon clear-notifications <item>|--all` and the orchestrator's
  `clear_notifications` tool resolve the rest (#194).
- Outcome notifications resolve when a settled item is viewed, so finished
  items no longer stay unread; `archon status` and `/status` list proposed
  lessons with the commands to accept or retract them. A work stage that
  needs no change can say why instead of shipping, and the run succeeds only
  after archond proves the branch has no commit beyond its base and a clean
  worktree (#207).
- Chat paste says why when the clipboard offers an image that could not be
  read or is of an unsupported type, instead of leaving the line unchanged;
  ids made in a row now sort in the order made, so same-millisecond feed
  notes and steer deliveries keep their order (#194).

### Stability

- archond no longer stops on stored or external state. Every
  background task is supervised and restarted with a growing pause (switched
  off with a notice after repeated failures); a corrupt run variable, a bad
  pipeline file, malformed `gh` output or an unknown Aether event fails or
  skips only the record it belongs to, which `archon doctor` and
  `archon status` list as quarantined with the exact retry and cancel
  commands.
- archond no longer panics on every start for a done initiative whose member
  was re-run (#200). Follow-ups: an initiative that was done or failed
  re-opens when one of its members is retried (a cancelled one is retried
  with `archon initiatives retry`); maintenance of one initiative that hits
  a bug no longer stops the daemon or the other initiatives; a crash of
  archond leaves a marker that the next start logs, shows in `archon status`
  and `archon doctor`, and posts once to the operator chat; an `archon mcp`
  bridge older than its daemon restarts itself once Archon was upgraded, and
  `/upgrade` refreshes it; the question of a decided or withdrawn gate is
  closed with the gate.
- Archon's own aetherd taking longer than 10 s to answer at startup (its
  recovery of a big data directory) no longer fails the start with "the
  daemon did not answer ... it is still running". Archon waits while the
  aetherd's startup log keeps growing, and gives up only after two quiet
  minutes or 30 minutes in all.
- The ship stage no longer panics with "one URL" on git older than 2.46
  (#185, by @Markfds01 in #186).

### Runs, review and pipelines

- A run stops at dispatch when the repository's git identity no longer
  matches the recorded one, instead of being refused at ship after its work;
  the refusal on retry says how to fix it (#190).
- A review that no budgeted reviewer can run is refused before any work, and
  under a budget Archon picks reviewers that report their cost (#191).
- The orchestrator reads a repository's state before stating facts about it,
  and looks again for a mirror still being made (#192).
- Clearer refusals and hints from `repo add`, `queue refresh` and gates;
  `--stage-timeout` on `queue add`, `steer` and `retry`, and `stage_timeouts`
  on the `retry_item` tool (#193).
- `archon say` waits while aetherd restarts and names the next command;
  clearer id, unknown-command and `archon doctor` wording (#209).
- The swarm pipeline keeps worker caches and its plan out of the shipped
  diff, documents that it needs two agents, and a lesson with a colon is no
  longer dropped (#195).

### Dependencies

- CI's `taiki-e/install-action` 2.87.29.

## 1.4.1

Released 2026-10-20.

A small patch release on 1.4.0, with no store migration. The daemon protocol
is now 18 (it was 17), so `archon` and archond upgrade together.

Archon still asks Aether for protocol 24. Image paste and the notification
center need **Aether 1.2.1** or newer: it serves image attachments and the
`notifications` feature, and both were tested with it. Against an older
aetherd, such as 1.2.0, archond logs one line and posts no notifications;
everything else here works on every aetherd 1.4.0 worked with.

Images reach the orchestrator from `archon chat`. Ctrl+V (or Alt+V) attaches
a copied image as a chip, `[image #1]`, read through `wl-paste` or `xclip`;
with only text on the clipboard it pastes the text, and Ctrl+Shift+V stays
the terminal's plain-text paste. A copied or typed path to an image file,
`/image <path>` and `archon say --image <path>` attach files, and Backspace
after a chip removes its image. archond checks each message as Aether does
(at most 10 PNG, JPEG, GIF or WebP images, 3.8 MB each, 10 MB together),
delivers the images through Aether's `send_message` attachments (an aetherd
speaking protocol 19 or later), and keeps only `[image: name, size]` in the
feed. A text-only message is the frame it always was.

Steering several running items at once, or one item several times, no longer
panics archond ("steps are only added while one is made"). A steer's delivery
to a stage run and the sweep's could both make the same owed steps and then
both forget them; deliveries to one stage run now take turns, so each steer
is delivered exactly once.

A reviewer's verdict is accepted in any letter case: `PASS`, `Pass` and
` fail ` count as `pass` and `fail`, as do the same words in `review.verdict`.
Any other word is still refused.

Against an aetherd with a notification center (feature `notifications`),
archond posts every open operator question there, gates included, with its
suggested answers as actions, and posts blockers and outcomes without. An
answer given there is applied as `/answer N <choice>` is, under the same
rules, and a question closed anywhere else resolves its notification. An
older aetherd is left alone: one log line, no change.

Every pull request Archon opens or updates now carries one risk label,
`risk: low`, `risk: medium` or `risk: high`, and a "Risk" section at the
top of its body saying what the level means for merging, why it was chosen
and the evidence: the files and areas touched, the diff's size, the checks
run, the review and any screenshots. The implementing agent proposes the
level by a written rubric, a reviewer confirms or changes it, and archond
applies it at every delivery, creating the three labels where the
repository lacks them. `get_item`, `list_queue`, `queue list` and
`queue show` show the level. The label is advisory only: no merge, gate or
review rule follows from it, and a label GitHub refuses never fails a
delivery.

Dependencies: `serde_json` 1.0.154 and `tokio` 1.53.3, both patch releases,
and CI's `taiki-e/install-action` 2.87.28.

## 1.4.0

Released 2026-10-19.

Operator commands are easier to use and safer to grant: `/archon answer`,
`approve`, `reject` and `merge` work as whole-message commands, a host
command's applied or refused result shows in the TUI without an orchestrator
turn, and a GitHub operator's commands reach only the repository of their
thread. archond now supervises its own aetherd and restarts one that was
killed. 1.4.0 holds every 1.3.2 fix, so it upgrades 1.3.1 and 1.3.2 alike:
no store migration, daemon protocol 17 as before.

Archon asks Aether for protocol 24 and needs **Aether 1.2.0** for the whole
TUI experience. With an older aetherd it falls back to the newest protocol
that aetherd serves, down to 14: Aether 1.1.x (protocol 23) shows host
command results as notes led by `Applied:` or `Refused:` instead of coloured
by level, and an Aether before 1.1.0 has no notes, so archond logs one
start-up warning and the results stay in Archon's feed only.

### Added

- `/archon answer N …`, `/archon approve`, `/archon reject` and `/archon merge
  ITEM` work as `/answer`, `/approve`, `/reject` and `/merge`, with the same
  sender checks and results; the `/archon` usage error lists them (#109).
- archond adds a display-only note to the orchestrator's transcript when it
  applies or refuses `/answer`, `/approve`, `/reject`, `/merge`, `/upgrade`,
  `/watch` or `/archon`, so the TUI shows the result at once; protocol 24
  notes carry an applied or refused level (#141, #153, #155).
- `/approve` and `/reject` take a note; out-of-range option numbers and a
  malformed `q_` reference are refused (#142, #143, #150).
- The line chat handles `/upgrade` and `/watch` and lists them in `/help`
  (#146, #154).

### Daemon

- archond supervises its own aetherd in dedicated mode: when nothing listens
  on its socket for two seconds it starts aetherd through the CLI's lifecycle
  and start lock and says so in the feed. A failed start backs off from 10 s
  to 5 min, is posted as a blocker once per outage, and `archon status` names
  the failure and `archon daemon start`, which retries at once. An aetherd
  that listens but refuses the handshake is reported as refused. Shared mode
  is unchanged (#119, #171).
- `archon daemon restart` and `upgrade` load `config.toml` first and refuse
  with the parse error while archond keeps running; `daemon stop` refuses
  while runs are active, naming how many, unless given `--force` (#118, #159).
- Status and doctor compare the configured orchestrator model and effort with
  the running generation and report `configured X, running Y` as stale (#121,
  #160).

### Security and authority

- The orchestrator's repository mirrors are read-only on disk: archond
  clears every write bit of a mirror's work tree after each refresh, so the
  orchestrator's own edit, patch or shell write into `repos/<name>` fails
  on Codex and Claude Code alike, where the prompt alone did not hold. A
  change made anyway (after a `chmod`, or a commit through git) is undone
  at the end of the orchestrator's turn, posted to the feed and sent to
  the orchestrator as a waking `mirror` event (`reverted`). Remove a mirror
  by hand with `chmod -R u+w` first.
- A GitHub operator's commands are scoped to the repository of their thread:
  settings, merges, approvals and item changes of other repositories, and
  global settings, caps, pause and resume, are refused. A loosening change in
  the thread's own repository asks for confirmation (#135).
- `request_authority` needs a meaningful quote on word boundaries (3 words or
  12 non-space characters unless it is the whole message), and a gate
  answer's quote must contain the answer word (#140).
- In shared mode every operator chat command is refused visibly, in the feed,
  as a TUI note and as an unapplied trigger (#145, #156).
- Hardening: artifacts are copied through `O_NOFOLLOW` handles and symlinked
  files or directories are refused; an existing socket directory is
  tightened to 0700 (another user's writable one is refused) and sockets bind
  under umask 0077; fast checks drop `GIT_*` like full checks; `publish_report`
  refuses paths outside a registered repository or item workspace; `wget`
  runs with `--https-only` (#124, #157).
- The daemon's git disables `commit.gpgSign`, `tag.gpgSign` and
  `gpg.program`, so team merges neither need a signing key nor run a
  repository's `gpg.program` (#116, #139).
- Request authority is classified exhaustively, so a new request kind does
  not compile until it is classified; the authorization steps of the request
  handler are named and their order is tested. No request gains or loses
  authority (#130, #134, #169).

### Merge paths

- Items delivered onto another pull request are refused by `request_merge`,
  `merge=auto` and `MergeItem`, naming the pull request to merge by hand; the
  owed auto-merge is pinned to the delivered commit (#137).
- A pending merge confirmation closes when the pull request's head moves
  (#138, #161).
- Merges and refreshes read a run's retained base through one helper, so
  items forked from a non-configured base still merge after a prune (#117,
  #149).
- `repo add` records the checked-out branch when no base is given and
  refuses an empty repository, a detached HEAD, and a `ship = pr` repository
  without an `origin` remote (#136).

### Fixes

- A Codex reviewer that writes its `complete_task` call out as its final
  text (`{"summary": …, "structured": {verdict…}}`) has that verdict read,
  instead of failing the stage and costing a retry.
- Every orchestrator reply is kept when a second operator message arrives
  mid-turn (#144, #152).
- Saved caps survive restarts and their lifetime is documented; zero means
  unlimited everywhere, and disabled budgets are warned about (#151).
- `pipelines check` rejects empty prompts, `lessons stats` works after a
  repository with runs is removed, and titles refuse control and bidi
  characters (#122, #123, #125, #162).
- The CLI exits quietly on a closed pipe, validates pause, resume and id
  arguments, rounds costs consistently and labels time zones (#126, #128,
  #129, #168).
- GitHub reply marker stripping is idempotent, feed settings lines are
  sentences, and operator usage and confirmation wording is fixed (#127,
  #147, #148, #170).
- Dead code is removed, the longest non-authority functions are split, and
  archon-daemon's integration tests build as one binary (#131, #133, #172,
  #173).

### Docs

- The reference docs match the code, with consistency tests that hold them
  to it, and cover the autonomy keys and chat history (#132, #166).

## 1.3.2

Released 2026-10-19.

### Fixes

- `refresh_item` on an item queued `onto` a pull request merges that pull
  request's current head, not the repository base. When another delivery moved
  the head, so the item failed at ship, a refresh now keeps its run, review and
  history, and it delivers as a fast-forward of the pull request. The ship
  failure tells you to refresh the item instead of queueing new work (#111).
- An item queued `onto` a pull request now counts a review that read from the
  pull request's frozen head as a review of the whole change. The review gate
  used to start the change at the pull request's fork from its base, so such a
  review failed as "read only from" an unanchored commit (#110).
- A timed-out stage's gate message now reports how long the stage actually ran,
  rather than the pipeline's default timeout (#165).
- A swarm plan that Archon refuses (an objective over 8000 bytes, more tasks
  than the agent caps run at once, an interface written as a string) goes back
  to the planner in the same stage with every problem listed, at most twice,
  before the stage fails. The planner's brief now states the run's task limit,
  the byte limits and the exact plan shape up front (#167).

## 1.3.1

Released 2026-10-18.

Archon ships a builtin `swarm` pipeline for changes that split into
independent parts. A plan stage divides the change into tasks, each with its
own disjoint owned paths. Build workers then run in parallel, one per task,
and can mix engines: the model plan's `build` list is taken one entry per
worker in task order, so a `codex` worker can sit beside Aether ones. An
integrate stage merges the workers' results, runs the checks and reviews the
whole change. Peers message each other through Aether 1.1.1 when it is
available.

Try it:

```sh
archon queue add --title 'Add the export command' --pipeline swarm \
  --brief-file brief.md --model-plan-file plan.json
```

```json
{"plan": [{"tier": "strong"}],
 "build": [{"tier": "fast"}, {"tier": "fast", "provider": "codex"}],
 "integrate": [{"tier": "strong"}]}
```

See `docs/PIPELINES.md` for the plan format. Watch a run with `archon queue
show <id>`.

## 1.3.0

Released 2026-10-17.

Repositories can work in tiers: items fork from an integration branch and run
a fast check limited to what they change, archond verifies the integration
branch in the background, bisects a red run to the merge that broke it and
queues one fix, and `archon promote` moves a fully green integration branch
into the release branch and runs a confirmed release hook. Work can be queued
`onto` an existing pull request and is delivered into it with a fast-forward
push. Pull requests are tracked by number, a repository can run `gh` and its
pushes as its own GitHub user and refuse attribution lines, and `archon
release check` gates publishing. Tier-only stages are routed across providers
by weight and live quota headroom, and push notifications reach the operator.
The store migrates automatically on first start (migration 15); older Archon
binaries refuse the migrated store. Daemon protocol 16.

### Added

- Verification tiers per repository: `integration_branch`, `release_branch`
  (default `main`) and `[checks] fast` / `full`, read from a committed
  `.archon/repo.toml` or set with `archon repo set --integration-branch`,
  `--release-branch`, `--fast-check` and `--full-check` (registry values win
  field by field; any change needs the operator). New items fork from
  `origin`'s freshly fetched integration branch and their pull requests target
  it; a missing integration branch is created from the release branch. Every
  stage contract names the fast command and forbids the full suite.
  Repositories without tiers behave as before (#84).
- Affected-only fast checks: the fast check may name `{packages}` or
  `{packages:<flag>}`, filled with the Cargo workspace crates a change touches
  and their dependents (elsewhere, the top-level directories it changes).
  Before delivery archond runs the trusted check again on the committed tree,
  one at a time per host with a 30 minute limit, and refuses the item with the
  output's tail when it fails. Only the registry's command runs, or a
  committed `.archon/repo.toml` command the operator confirmed word for word
  with `archon repo set --confirm-fast-check` (#89).
- Integration branch verification: a new archond worker verifies each
  integration branch's head after a batch of merges or once it is idle, from
  the commit's GitHub check runs when CI covers it, else by running
  `checks.full` in a sealed daemon-owned checkout under one build slot per
  host. A red head is bisected over the first-parent merges since the last
  green commit, one fix item naming the culprit is queued onto the integration
  branch, and the operator is notified. `status` and `queue_status` show each
  branch's health, last green commit and the merges since. A committed full
  check runs only once confirmed with `archon repo set --confirm-full-check`
  (#88).
- Promotion: `archon promote <repo>` and the `promote` tool merge the
  integration branch into the release branch with a merge commit once the
  full check is green on its exact head, then fast-forward the integration
  branch. A red, verifying, unverified or moved branch is refused with the
  reason; an orchestrator without `grant_merge` stops at the open pull
  request. An optional release hook (`archon repo set --release-hook`, or
  `[checks] release` confirmed with `archon repo set --confirm-release-check`)
  runs at the merge commit. A promotion a merge queue held is finished by
  promoting again once it landed (#90).
- `onto` work for an existing pull request: `enqueue`, `queue add --onto` and
  initiative members take `onto: {item | pr | branch}`. archond freezes the
  pull request's identity, head and base at enqueue and refuses closed, merged
  or fork pull requests, ones the operator cannot push to, local-ship
  repositories and `onto` with dependencies (#96). Delivery pushes the
  reviewed commit onto the pull request's head branch as a leased
  fast-forward, never forced, to a destination proved to be the head
  repository, and confirms the new head; the run ends `SHIPPED: updated #<n>
  to <sha>`. The pull request's title, body and base are left alone, and
  stacking on or refreshing a delivered `onto` item is refused (#97).
- Per-repository GitHub user: `archon repo set NAME --gh-user LOGIN`
  (operator only; `--gh-user ""` clears it) runs that repository's `gh` calls
  and pushes with the login's token from `gh auth token`, given only to that
  call and never logged or stored. A login without a token or a non-github.com
  origin is BLOCKED, with no fallback to the daemon's account. `archon doctor`
  checks each login. Fetches and the GitHub watcher keep the daemon's login
  (#85).
- Attribution policy: `archon repo set --attribution` (or `/archon set --repo
  R attribution=...`) takes `item-id` (the default) or `none`. Under `none`
  the PR body has no `Archon item:` line and delivery refuses a PR body or
  commit carrying a `Generated with` footer or `Co-Authored-By:` trailer,
  quoting each line (#75).
- `archon release check [owner/repo]` audits release health: version tags,
  each platform's archive and `.sha256`, `SHA256SUMS`, `VERSION`, the highest
  stable version as Latest and whether the latest completed CI run got a
  runner; `--json` is supported and `archon doctor` runs it for Archon and
  Aether. The release workflow runs it with `--pending-tag` and `--assets`
  before creating the draft, so a broken release history blocks publishing
  (#92).
- Provider routing for tier-only stages: `[models] provider_weights` (with a
  per-repository overlay and `default_provider_weight`) picks one provider per
  item by a deterministic weighted draw, kept while it meets each stage's
  capability floor; `get_item` shows the routed provider, and `archon cost
  --by provider` and `model_evidence` report each provider's share (#78). The
  weight is scaled by the live quota headroom Aether reports; `[models]
  reserve_percent` and `interactive_providers` keep part of each window back.
  An item out of headroom moves to another provider (recorded as
  `provider_switch`), and when every provider is out it is held until the
  earliest reset. `status` and `queue_status` show each provider's
  headroom, reserve and next reset (#80).
- Push notifications: a `[notify]` section sends gate, question, failure,
  blocked, pr_checks and budget events through a command (such as `ntfy`) or
  a webhook, rate-limited by `min_gap_secs`. Events carry only an id, the item
  title and a line Archon writes (#93).
- Default reviewers: reviewers with no `<stage>.review` plan use the
  `[review]` profile (`default_tier = "balanced"`, `default_effort = "low"`,
  `prefer_other_provider = true` by default), resolved from the catalog at
  spawn under model policy (#95).
- Every new orchestrator generation first receives an `orchestrator_briefing`
  built by the daemon from its store: unfinished items, recent outcomes, open
  questions and gates, initiatives and recent chat (#76).
- Orchestrator freshness: `archon status`, `archon orchestrator status`,
  `queue_status` and a new `doctor` check show the generation and whether its
  prompt, tools or lessons are outdated, so an owed restart is visible (#87).
- `archon autonomy` and the `autonomy_policy` tool list every structural
  limit with its value, default, ceiling and source, and the directives in
  force as n/20; a refused directive names the oldest and soonest-to-lapse
  ones to clear (#94).
- `operator_answer` triggers carry the question they answer, labelled with its
  number, so a delayed answer stays understandable (#79).
- `archon tui` puts Archon's operator commands (`/answer`, `/approve`,
  `/reject`, `/merge`, `/archon`, `/status` and the rest) in the Aether TUI's
  `/` menu, through `aether tui --host-commands`, in dedicated mode
  (`/pause` and `/resume` stay Aether's own there). An Aether
  without that option prints a warning and starts without them (#102).
- Under `grant_merge`, a reviewed request with pending checks waits for them
  and then merges without asking; a failed check or a 60-minute wait asks the
  operator. A new operator-only repository setting `merge_waive_checks` (for
  example `["next"]`) lets reviewed pull requests into those base branches
  merge under `grant_merge` without waiting for checks (#103). See
  `docs/configuration.md` and `docs/operator-authority.md`.

### Changed

- Pull requests are tracked by number and repository instead of branch name,
  in a new `run_pull_requests` table; settled state is keyed
  `pr_settled:<host>/<owner>/<name>#<number>`. Restack and refresh act only on
  the run's own pull request (#81).
- archond serves hello, reads and status (with a `recovery` progress section)
  while recovery runs; a change waits up to 2 s for recovery and is then
  refused with a typed `recovering` error to retry (#83).
- The orchestrator prompt carries its 3.6 KB GitHub section only when the
  watcher is wanted (#91).
- CI runs on pushes to `main` and `next`; pull requests into `next` run only
  lint, those into `main` the full suite; runners are pinned to
  `ubuntu-24.04` (#64, #86).
- `/a` is gone: answer with `/answer N …`. archon chat and archond reply to
  `/a` with "use /answer", and every hint that offered `/a` names `/answer`
  (#102).

### Fixes

- `queue_status` answers from a bounded local snapshot: Aether's sections and
  the disk probes share one deadline and stale sections are named with their
  age, so a slow aetherd or filesystem no longer stalls status. The waiting
  list is paged (50 by default, 500 at most) (#77).
- A repository whose open pull requests cannot be listed is asked once per
  observation pass, with one coalesced warning (#74).
- Default reviewers: `[review] default_tier` is a preference. Under a model policy
  that permits nothing at the tier, the reviewer rule names the best permitted
  model (or the lead's own) instead of sending the agent to `list_models`, which
  ended in cancelled delegations (#101).

### Upgrading

The store migrates to schema 15, which adds `run_pull_requests` and converts
each run's verified pull request URL into a number-based identity; legacy
branch-keyed settled entries are rekeyed when the branch maps to exactly one
pull request, and the rest are asked again. The migration is one-way: a 1.2.5
`archond` refuses the migrated store. Back up `archon.db` first, and stop the
1.2.5 `archond` before updating the binary with `archon update`, since the
daemon protocol is now 16; see
"Back up before a release that migrates the store" in
[docs/install.md](docs/install.md).

## 1.2.5

Released 2026-10-16.

Review-holding leads now get reviewer-model advice based on the repository's
policy when they spawn, and recovery of running work no longer depends on
reading repository configuration. Local release builds can now produce the
tagged archives and checksums without GitHub Actions. Daemon protocol remains
13, and no store migrations changed.

### Added

- `scripts/build-release-local.sh <tag> <outdir>` builds release archives,
  `SHA256SUMS` and `VERSION` from a tag without GitHub Actions. Darwin targets
  need `SDKROOT` pointing at an Apple SDK; see [development](docs/development.md)
  (#62).

### Fixes

- A review-holding lead's reviewer-model advice follows the model policy as it
  stands when the lead spawns, so narrowing a repository's policy mid-run
  reaches later leads. Recovering a running item no longer reads the
  repository's config, so a malformed one no longer fails work that was
  already running (#61).

## 1.2.4

Released 2026-10-15.

Model plans speak in tiers: with Aether 1.0.9 a stage can ask for `fast`,
`balanced`, `strong` or `frontier` and Archon resolves the model from Aether's
catalog within model policy. Operators approve gates and change safeguards
from chat, the TUI or GitHub; ask for a merge in chat and it lands on current
main; new branches fork from a freshly fetched base and `refresh_item` brings a
stale pull request up to date. Upgrades drain first, builds are lean and per
item, and logs, reports and artifacts expire. Codex work stays priced after
Aether's subscription estimates. The store migrates automatically on first
start (migrations 13 and 14); older Archon binaries refuse the migrated store.
Daemon protocol 13.

### Added

- Tier-based `model_plan`: an entry may be `{tier, effort, provider?}`
  instead of `{model, effort}`, with tiers `fast`, `balanced`, `strong` and
  `frontier` (`strongest` is an alias of `strong`). Archon resolves them from
  Aether's `list_models` tiers within model policy at enqueue, update, retry
  and dispatch, prefers the orchestrator's provider unless one is named, falls
  back downward and records the fallback. get_item and the CLI show the tier
  and the resolved model. Defaults resolve the pipeline nudge's tier, capped
  by `[models] max_default_tier` (default `strong`; a repository may only
  lower it). Exact pins and stored plans behave as before (#59).
- Operator authority from every channel: proposal and report gates, and the
  supported safeguards, can be approved or changed from the Aether TUI, remote
  chat, `archon chat` or GitHub comments with operator standing. Plain-language
  loosening needs a daemon confirmation bound to the originating channel.
  Agent messages, relays and outside GitHub requests confer no authority.
  Settings persist globally or per repository with an Authority audit feed and
  guarded undo, and the new `guide` tool serves the authority, settings and
  GitHub topics (#55).
- `/merge` in chat merges an item's pull request on the operator's request,
  pinned to the reviewed head. Requests GitHub queues are kept and settled when
  the merge lands or the pull request closes; confirmations settle atomically
  with the merge (migration 14) (#51, #57).
- Reviewer and timeout control: `model_plan` names delegated reviewers through
  `<stage>.review` under model policy, stages declare effort defaults (a higher
  effort needs a stated reason, and get_item marks it), and `stage_timeouts`
  on enqueue, update_item and steer_item bound each stage (#47).
- Fresh bases and `refresh_item`: in `pr` mode a new branch forks from a
  freshly fetched `origin/<base>`, leaving the operator's checkout untouched.
  `refresh_item` (`archon queue refresh`) merges a newer base into a finished
  item's branch and ships into the same pull request. Pull-request checks that
  never start (billing, no runner) raise a Blocker separately from checks that
  failed (#41).
- Drained upgrades and restarts (`archon daemon upgrade`, `daemon restart`
  and `/upgrade`), running/installed/latest versions in status, and explicit
  `status --check-updates` release checks (#37, #43). Daemon protocol 11.
- `repo add --github` and `/watch` register GitHub watching for a repository
  through Archon's dedicated Aether (#37, #43).
- Sustainable filesystem retention: detached daemon logs rotate by size and
  age (10 MiB, three archives, 14 days by default), settled item report/Lavish
  snapshots expire after `[storage] retention_days = 14` with live work, gates
  and questions protected, and status / `queue_status` report bytes by storage
  kind and the last GC's freed bytes. SQLite history and spend remain intact.

### Changed

- Codex usage is an estimate since Aether 1.0.9: a Codex turn bills $0 and
  carries its list-price dollars as `estimated_cost_usd`. With the default
  `count_estimated_cost = true` budgets count it as before, so a Codex-heavy
  item stays bounded; with it off, Codex counts $0 toward budgets. Learning,
  model evidence, a finished item's reported cost and benchmark costs now
  always use the API-equivalent total, billed plus estimated, so a Codex
  stage never looks free. `archon cost` and `cost_report` add
  `≈ $X API-equivalent` when budgets leave estimates out. Older records keep
  their list-price cost and read as before, and outcomes captured while
  budgets left estimates out are re-priced with them when learning reads them.
  A benchmark cost stays marked when unpriced work has no estimate covering it.
- Agent cargo builds are lean and per item: each item builds into
  `<data>/cache/<repo>/items/<item>/cargo-target`, kept across retries and
  deleted by archond when the item ends (unreadable subdirectories included);
  `archon gc` sweeps ones whose item is final or gone, and the feed and
  `archon status` report the bytes freed. archond's cargo config turns off
  incremental builds and uses line-table debuginfo for dev and test. The
  shared per-repository directory is now opt-in (`[worktrees] shared_cache =
  true`); `[worktrees] item_cache`, `lean_builds` and the per-repository
  `[cache] item`, `lean` switches turn the rest off. `GcReport` gains
  `item_caches` (defaulted, so older clients still decode it).
- Artifact retention defaults to 14 days. Existing tighter worktree/cache
  defaults stay unchanged; zero durations disable their configured expiry.

### Fixes

- A review-holding stage whose plan names no reviewer models is told to use
  the models its item's policy allows other than its own, within the effort
  cap, or its own model when the policy allows no other. Before, it was told
  to pick a cost-reporting model, which single-model items (the fast-tier
  smoke preset, or a repository narrowed to one model) could not follow, and
  their reviewers were cancelled (#58).
- A review holder whose plan names no reviewer models is now told to use a
  model its item's policy allows: another than its own where there is one,
  else its own. It was told to find a different priced model, which a
  single-model allowlist forbids, so such items failed or delegated reviewers
  Archon then cancelled. The stale claim that Codex usage arrives unpriced is
  gone.
- Refreshes map steered build/implement models and reviewers onto the refresh
  pipeline, fill missing choices within model policy, and skip proposal and
  report gates while keeping review, merge and tools. Refreshing a finished
  run (including a retry) works after its prompt templates expire: the base
  branch is retained separately, with the configured repository base used
  for older runs whose metadata is already gone.
- Upgrades drain before replacing binaries, keeping the old CLI and agent
  tools compatible with archond while running stages finish. Dispatch stays
  held during installation; an expired drain installs nothing unless requeue
  was requested.
- Update checks probe both programs' installed and latest versions concurrently,
  with a 15-second limit per probe within the client's 60-second timeout.
- The multi-repository currency benchmark runs its ledger through Cargo,
  honoring worktree configuration and `CARGO_TARGET_DIR`.
- Reviewers' experiment copies now go in a directory Archon owns on its data
  disk, `<data>/experiments/<item>`, instead of wherever `mktemp -d` puts
  them, which is often a small `/tmp` the whole machine shares. Archon creates
  the directory before a stage that can review starts and names it in the
  contract. It deletes the directory once the item has finished and its
  agents, and every agent they started, have stopped. `archon gc` removes any
  a crash left behind, and never follows a symlink there.

## 1.2.3

Released 2026-10-15.

Your corrections now teach Aether's memory: with Aether 1.0.8, a quality
correction tells Aether which recalled memories concerned the corrected stage,
and a memory you name is ranked a little lower. Stages of a repository share
one build cache, and a single switch turns dollar budgets off. Reviews retry
less and finish sooner. The store migrates automatically on first start
(migration 12); older Archon binaries refuse the migrated store, so keep 1.2.3
once you have run it. Daemon protocol 9.

### Added

- Workspace hygiene: stages of a repository build into one shared cargo
  target directory (`<data>/cache/<repo>/cargo-target`, opt out with
  `[cache] shared = false`), `archon gc` and the periodic pass also remove
  idle shared caches and expired artifacts and report the bytes freed, and
  stages publish files (`publish_artifact`, `list_artifacts`,
  `read_artifact`) to a size-bounded per-item store that outlives the
  worktree; dependents are told its path.
- `[limits] budgets_enabled = false` (or `archon queue caps --budgets off`,
  until archond restarts) turns every dollar budget off: item budgets, the
  daily and total admission holds and the distillation reservation stop
  enforcing, no `cost_budget` goes to Aether for new stage agents, and
  usage without a dollar cost (every Codex model) no longer blocks a run.
  Spend is still recorded and reported, `archon status` and `queue_status`
  say `budgets: off`, and a `budget_usd` set meanwhile is accepted with a
  note that it is not enforced. The default is unchanged. Daemon protocol 9.
- Quality corrections tell Aether which recalled memories hurt the corrected
  stage. Delivery happens in the background and resumes after a restart or
  reconnect. Undo retracts the feedback, and `learning why` shows delivery
  progress and reasons for skipped or unconfirmed feedback.

### Fixes

- A review holder that named only its latest delta reviewers no longer fails
  its stage for not naming the earlier whole-change review they rest on.
  Archon now finds that anchor among the reviewers the holder itself delegated,
  checks them exactly as named ones (a cancelled review, a commit the branch
  does not hold or an unanswered fail still refuses), and records them as
  `review.implicit_reviewer_agents`. Only named passes count toward the review
  level. It looks only for an agent that ran no other attempt of the run, once
  none of its reviewers is still running, and refuses when Aether cannot show
  one of their results.
- Reviewers run each experiment's repeats under one 30-second limit, not one
  limit per repeat. Their scratch copies hold tracked files only (no build
  outputs), come from `mktemp -d` instead of a literal `/tmp` path, and are
  deleted afterwards. The contract now says outright that the project's own
  build and tests run uncapped. In a benchmark, one reviewer's 40 capped runs
  outlasted the whole task, and agents kept filling the shared `/tmp`.

## 1.2.2

Released 2026-10-15.

With Aether 1.0.6, agents in registered repositories now share memory with your
direct Aether sessions in the same checkout. Review rounds are faster without
being weaker, and the bench reports timed-out work and incomplete costs
honestly. No store migration: update in place.

### Added

- Agents in registered repositories share memory with direct Aether sessions using
  the registered checkout (with Aether versions that advertise `repository_root`).
  Old memories under the short name can be moved with
  `aether memory alias <name> --root <checkout>`.

### Fixes

- A run whose registered checkout has gone missing fails with the path and how
  to recover (restore it there and retry the item), instead of retrying.
- Review rounds are faster without being weaker. Reviewers run each ad-hoc
  experiment, such as a mutant, under a 30-second timeout, and a hang counts as
  caught. On a blocking finding the agent cancels the reviewers still reading
  and fixes it at once. Each cancelled reviewer is replaced by a new review of
  the whole change; a finished review is checked again only on the fix. Tests
  of blocking code must fail in seconds instead of hanging.
- A reviewer's verdict that marks an abbreviated commit with a trailing `...`
  or `…`, such as `"head": "5d240fa..."`, is now accepted. Before, a passing
  review written that way failed the stage. The id must still name a commit
  in the run's workspace.
- A `model_reasons` key that is the pipeline name, such as `direct`, instead of
  a stage id such as `implement`, is still refused. The error now says that
  keys are stage ids of the chosen pipeline and lists them.
- A benchmark trial that hits its deadline keeps its verdict. For Archon, its
  last committed branches are now graded as well and reported apart as
  "graded on timeout", never as a pass. `quality-ledger-transfer` now has 20
  minutes for every arm.
- The bench's grading on timeout failed every time: the verdict's grading left
  its git worktree registered, so the second checkout at the same path was
  refused. Grading now prunes it.
- A review holder is told to name every reviewer its coverage rests on,
  earlier whole-change passes included. Naming only the last delta reviewers
  was refused and cost a retry.
- Reviewers run each mutant or probe in a copy under `$TMPDIR` (else `/tmp`),
  so experiments honour a private temporary directory and never touch the
  shared workspace.
- The bench now marks a cost with `≥` when Aether reports it incomplete
  (`cost_gap`, such as a parked Claude Code coordinator), instead of showing it
  as exact. A total or mean that also includes estimates is marked `*`, since
  an estimate is no bound; `chart.json` and the history CSV carry it as
  `cost_bound`.

## 1.2.1

Released 2026-10-15.

You can now tell Archon that a stage of a finished run got it wrong. For a
quality correction it can attribute, it then trusts the model that did that
work less for that kind of work in that repository, and it can retire the
Aether memory behind the mistake. Every correction can be undone, and review
is never lowered. Agents can also start with a lean host context, which needs
Aether 1.0.5. This release migrates the store one-way: read "Upgrading"
before you update.

### Added

- `archon learning correct qi_… --run prun_… --stage <stage> --cause <cause>
  --reason <text>` records the operator's correction of one stage of one
  finished run. A `quality` correction counts that stage's one producing
  model choice as unsuccessful, once, in its repository and routing context,
  for 90 days; an ambiguous history is recorded with no model penalty, and
  the receipt says why. `archon learning why`, `undo`, `restore-memory` and
  `retry-memory` show and reverse it. Agents are refused.
- `--forget-memory mem_…` forgets that one Aether memory in the Aether
  instance `archond` last connected to, which must speak Aether protocol 15
  or newer, and keeps a receipt. Archon stores the memory's ID and state,
  never its text.
- A repository cohort with a correction reads only runs of the last 90 days,
  never pools global evidence, and explores no other model while a
  correction is active.
- Add opt-in `[aether] host_context = "lean"` for new agents. On supported
  Aether versions, Claude Code skips skills and non-Aether host MCP servers;
  Codex ignores the setting. Full remains the default. Older daemons receive
  no new wire field, with one log warning and a doctor check explaining the
  fallback. Stage records and model evidence retain the effective context.
  On the bench's smoke tasks with Claude Haiku, lean passed as many tasks
  as full (13 of 16 each), with 33% less reported input and 30% lower
  known cost. One lean run's accounting is incomplete, so these figures do
  not establish the true all-in saving.

### Fixes

- `archon-bench --dry-run` no longer reads the version of the default
  `aetherd`: it takes its sockets only from the private sandbox the
  preflight checked, and probes nothing without one.

### Upgrading

The store migrates to schema 11, which adds the `operator_feedback` table.
The migration is one-way: once the new `archond` has started, a 1.2.0
`archond` refuses the store ("store schema version 11 is newer than this
archon supports (10)"). Back it up first:

1. `archon daemon stop`. `archon` and `archond` speak protocol 8, so the
   1.2.0 `archond` must stop before the binary changes.
2. Copy `archon.db` from the data directory (`~/.local/share/archon` by
   default) to a name of your own, such as `archon-1.2.db`. If an
   `archon.db-wal` lies beside it, copy that too, as `archon-1.2.db-wal`.
3. `archon update`. The next command that changes something starts the new
   `archond`.

To go back to 1.2.0, see "Back up before a release that migrates the store"
in [docs/install.md](docs/install.md): stop `archond`, run `archon update
--version v1.2.0` and put the copy back. What the new release recorded since
is lost, its corrections included. A memory a correction forgot stays
forgotten in Aether: run `archon learning restore-memory` before going back,
or restore it later with Aether's `/memory` (`u`).

## 1.2.0

Released 2026-10-14.

1.1.0 was not published on its own, so this release carries all of it: read
[its notes](https://github.com/mengsig/archon/blob/v1.2.0/CHANGELOG.md#110)
too. On top of 1.1.0, one `archond` serves a data directory, a command that
only reads no longer starts one, and the orchestrator spends fewer turns and
less waiting on each request. On the bench's smoke tasks, with Claude Haiku,
the orchestrator's relay of an outcome fell from 9.9 to 4.6 seconds with no
task failing.

### Upgrading

- From 1.0.x: follow "Upgrading" under 1.1.0. Its store migration is
  one-way, so back up `archon.db` first.
- From a 1.1.0 build: `archon daemon stop`, update the binary, then
  `archon orchestrator restart`, which starts the new `archond` and gives the
  orchestrator the new prompt. An installed binary updates with `archon
  update`; one built with Cargo is rebuilt from the v1.2.0 source.
- A command that only reads, such as `archon status`, now exits 1 when
  `archond` is not running: start it with `archon daemon start`.
- Keep the config directory, data directory and socket all at their
  defaults or all elsewhere. A command that would start `archond` over a
  mix fails, even when all three variables are set; symlinks and `..` are
  resolved first. `archon daemon run` still starts, with a warning.

### Fixes

- Two `archond`s could run on one data directory under different sockets,
  and each undid the other's work. `archond` now holds a lock on
  `<data>/data.lock` from before it opens the store until it exits, and
  writes its pid and socket into it. A second one exits with an error that
  names them. `archon gc --offline` and tools that open the store to write
  hold the lock shared, so they and `archond` exclude each other; read-only
  opens, such as `archon lessons` without `archond`, take no lock.
- A command that only reads (`status`, `questions`, `log`, `cost`,
  `model-evidence`, `queue list`, `queue show`, `queue wait`, `queue caps`,
  `repo list`, `initiatives`, `autonomy`, `github-watcher`, `orchestrator
  status`, `reports`) no longer starts `archond` or Archon's `aetherd`.
  Without a running `archond` it prints ``archond is not running (start it
  with `archon daemon start`)`` and exits 1. Commands that change
  something, the TUI and the chat still start both, and now say so on
  stderr with the data directory and socket. `archon mcp` no longer starts
  `archond` for a read-only tool call either. The guide lists the rule under
  "Which commands start archond".
- Where the config directory, data directory and socket mix default and
  other locations, such as a private config and socket over the default
  data directory, no command starts `archond` automatically any more: one
  started there would serve the default store under the private socket. Such commands fail and
  name the variables, reads add the same note, `archon mcp` does not
  autostart, `archon gc` does not collect offline, and `archon doctor` warns
  in a new `locations` check. `archon daemon run` still starts, with a
  warning. Set all three together, or none. A symlink or `..` that leads to
  a default location counts as that default.

### Changes

- `archon status --no-start` is what `archon status` does now. The flag is
  still accepted and hidden.
- The one-time repair of an old `archon` MCP registration in Archon's own
  Aether instance now runs on the first command that starts `archond`
  (`archon daemon start`, or any command that changes something), not on a
  read.
- The orchestrator hears of the last unfinished item's outcome without the
  two-second coalescing window, as no other outcome is coming to share the
  turn.
- The orchestrator queues a clear change without first fixing or testing
  it, and relays a success without checking it again with git. Its prompt
  names `autonomy.review`, which it set at the top level and was refused.
  A brief now starts with the request's own words.
- The orchestrator is told its workspace is its working directory, and to
  read a repository's code in its mirror, never searching the disk for it.

## 1.1.0

Not published on its own: its changes ship in 1.2.0.

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
3. `archon update`. It also says when your Aether is too old (`aether
   update`).
4. `archon setup`. With Archon's own Aether instance, the first `archon`
   command repairs the `archon` MCP server and restarts that `aetherd` once.
   Shared mode: `archon setup` registers the server when it is missing and
   says what an existing one lacks; add `send_caller_meta = true` to it in
   your Aether config and restart your `aetherd`.
5. `archon aether mcp refresh archon` (shared mode: `aether mcp refresh
   archon`), so Aether lists 1.1.0's tools, then `archon orchestrator
   restart`. `archon doctor` reports a registration that still lists other
   tools than this `archon` serves, and the refresh that fixes it.

Eight built-in pipelines are gone (see "Breaking changes"). An item queued
on one fails when it would start, naming the pipeline to use: let such items
start before you update, or enqueue them again on `direct` or `adaptive`.
Point a repository's `default_pipeline` that names one at its replacement.

To go back to 1.0.1, see "Back up before a release that migrates the store"
in [docs/install.md](docs/install.md): stop `archond`, install 1.0.1 and put
the copy back. What 1.1.0 recorded since is lost. A running orchestrator
keeps the prompt and tools it started with until `archon orchestrator
restart`: until then it cannot delegate, steer, stack or use `adaptive`,
`direct` or its mirrors.

### Breaking changes

- **Eight built-in pipelines are folded into `direct` and `adaptive`.**
  `quick-pr`, `default-pr-review` and `strict-pr-review` are `direct` at
  review level `none`, `standard` or `strict`; `bugfix`, `docs`,
  `test-coverage` and `dependency-upgrade` are `direct`; and `team-pr` is
  an example to copy, `docs/examples/pipelines/team-pr`. Naming one is
  refused with its replacement unless a pipeline of yours has that name. A
  run already going finishes on the pipeline it started with. An item that
  names no pipeline runs `direct`, at the level it asks or your default
  review level, so with no knob set it is reviewed once, as on
  `default-pr-review`; the review level no longer picks a pipeline.
  `refactor`, `performance` and the pipelines that do not ship stay.
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
  tagged `notify` and GitHub requests still wake it, as in 1.0.1, but a
  routine success while other items are queued or running waits, up to 5
  minutes, for the last of them, so one turn reports them together. The feed
  shows each outcome at once. The new `[orchestrator] wake_on_success =
  false` leaves routine successes for its next turn.
- In dedicated mode `archon daemon stop` stops Archon's own `aetherd` after
  `archond`; `--keep-aether` stops only `archond`. `--aether` and `--all`
  still work, and are still refused in shared mode.

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
- New worktrees and read-only snapshots are checked out through that same
  git directory, so no hook and no filter that a repository or an agent
  configures runs as `archond`. Your own global filters, such as Git LFS,
  still apply.
- Untracked caches (`__pycache__`, `*.pyc`, `.pytest_cache`, `.mypy_cache`,
  `.ruff_cache`, `node_modules`, a top-level `target`, and
  `worktrees.cache_dirs`) no longer keep a finished worktree from being
  removed. Untracked source still does. A work stage is told to commit no
  build cache either: parallel branches that each committed `__pycache__`
  conflicted.
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
- **New built-in `direct`**: one agent implements a change one author can
  make and check, whatever its size, runs the checks it affects and
  commits. It is the cheapest pipeline that ships, and the one an item that
  names no pipeline runs. There are 10 built-ins.
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
  delta, also when the review of an earlier fix failed. A review of an amended commit supersedes the verdict on the commit
  it replaced, instead of failing the run.
- **A model plan is optional.** A stage left out gets the learned choice,
  else the orchestrator's model where your policy allows it, else the first
  allowed model. The item shows the filled plan.
- **A stage can hand a task back** with `ESCALATE:` (too large or risky for
  its pipeline, with a recommendation) or `BLOCKED:` (what it needs from
  you). Nothing ships, and the orchestrator moves the item to a pipeline
  that fits.
- Reviewers and verifiers rely on a full-suite result only when it names
  the tree they review (`git rev-parse HEAD^{tree}`), and otherwise run the
  suite themselves.
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
  with its next turn, and a routine success waits for the work still
  running. Urgent news (your answer, a blocker, an opened gate, report
  feedback, an escalation, a budget hold) is delivered at once instead of
  waiting out the coalescing window. A message of yours reaches it after
  the news held for it: `archond` delivers that news first, waiting up to
  10 seconds.
- It sends any change one author can make and check to `direct`, whatever
  its size, and sets the review level by risk: none when the change's own
  tests prove it. It queues a clear change without reading the code first,
  and relays a success in a line, without checking again what `archond`
  checked.
- `list_pipelines` shows a line per pipeline, and a pipeline named in the
  call in full: a few hundred bytes instead of several kilobytes.
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
  holds its queued members. Members build at once against the contract
  unless one waits (`after`) for another's code, and `initiative_finished`
  carries each member's outcome, so no member needs reading.
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
  of the higher of that and the list price. A stage resumed on a kept agent
  is charged only what that agent and its children spent during that
  attempt, not their earlier attempts again.

### Setup

- `archon setup` writes a `config.toml` with every key commented out when
  there is none.
- `archon doctor` warns about a pre-1.0 `archon.toml`, which is still not
  read, and reports an `archon` MCP server whose tools differ from the ones
  this `archon` serves, with the refresh that fixes it.
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
