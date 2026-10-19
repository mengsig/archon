# Changelog

## 1.4.0

Released 2026-10-19.

Operator commands are easier to use and safer to grant: `/archon answer`,
`approve`, `reject` and `merge` work as whole-message commands, a host
command's applied or refused result shows in the TUI without an orchestrator
turn, and a GitHub operator's commands reach only the repository of their
thread. archond now supervises its own aetherd and restarts one that was
killed. No store migration; daemon protocol 17 as before.

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

- An item queued `onto` a pull request counts a review that read from the
  pull request's frozen head as a review of the whole change (#110).
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
