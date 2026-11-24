# Upgrading Archon

How to move an installed Archon to a newer release, and what each release
that needs more than `archon update --yes` changes. The update command
itself is in [install.md](install.md#updating), and every release's changes
are in the [changelog](../CHANGELOG.md).

## From 1.5.x to 1.6.x

Archon 1.6.4 requires Aether 1.4.2 or later. Upgrade an older Aether with
`aether update --version v1.4.2` and restart aetherd, then run
`archon update --yes --version v1.6.4`. Upgrade Aether first: the update
from 1.6.3 runs 1.6.3's own Aether step, which does not pin 1.4.2.
Confirmed updates run by 1.6.4 select Aether 1.4.2 only for an older
installation; at 1.4.2 or later they use
Aether's normal update, without pinning it to an older release. Keep a backup before the first daemon start: approved intent
adds persistent requirement and clarification records. Existing queue items
without intent keep their previous behavior.

From 1.6.4 a running orchestrator whose prompt or tools changed in the
upgrade is replaced by a fresh generation at its next idle moment, with the
same hand-off as `archon orchestrator restart`, so `archon status` stops
showing "prompt outdated" without a manual restart; while the refresh waits
or fails, it shows "upgrade refresh delayed" and why. Stage `info` messages
no longer wake it, and its context is compacted earlier
([configuration](configuration.md)). None of this migrates the store. Going
back to an older binary is not supported once a release's store migration
has run, as an older archond refuses a newer store; undo these behaviours
with their switches instead: `quiet_info = false`, `compaction_tokens = 0`,
`idle_compact_after_minutes = 0` and `refresh_on_upgrade = false` under
`[orchestrator]`. A compaction threshold already sent stays with the running
generation until `archon orchestrator restart`.

From 1.6.4 a stage with a verdict calls the new `submit_verdict` tool before
it finishes, and is asked once to restate a verdict Archon cannot read
([PIPELINES.md](PIPELINES.md#the-stage-contract)). It needs no Aether change
and works with Aether 1.4.1 and later; `archon setup` adds the tool's class
to the `archon` server entry, and an entry without it still serves the tool.
A stage agent sees the tool once its `archon mcp` is the upgraded binary: a
server Aether starts per agent is, from the next stage on, and one shared
`archon mcp` started before the upgrade is not until it is restarted. A
stage whose agent does not have the tool finishes as before. The built-in
`e2e-testing`, `e2e-strict` and `release-journey` now go back to `setup`
only for a verdict that reads as `fail` with a finding whose `"fault"` is
`"environment"`. A test stage that failed its gate any other way (a `fail`
naming only the product, a verdict Archon could not read, a timeout, an
agent that failed) fails the run there, where 1.6.3 re-ran setup once
first. A pipeline of your own
keeps its loop unless it sets `on_fail.when`; a copy of a built-in in
`.archon/pipelines` keeps its old loop until you add
`when = "environment_finding"`. This adds no store migration either: stored
stage runs gain an optional `restatement` field an older archond ignores,
and the same rule about going back to an older binary applies.

### Effort sizing on by default

From 1.6.4 `[models] effort_sizing` is on unless the global config turns it
off ([configuration](configuration.md)). The orchestrator and every
delegating lead size each effort by one rubric, `low` included; a reviewer's
effort, from `[review] default_effort` (now `medium`) or a `<stage>.review`
plan, is a default the lead may lower for a small, low-risk diff rather than a
floor; a stage the plan leaves out, or that a knob or a steer inserts, takes
its own effort within the policy's cap rather than the pipeline tier's or the
first stage's; and the built-in stage nudges are served (by `list_pipelines`
and `archon pipelines list`) worded by the rubric.
A config that already set `effort_sizing = true` gets 1.6.4's rubric too:
reviews of small low-risk diffs move to `low`, and inserted stages and the
default `[review] default_effort` change as above.

To keep 1.6.3's effort rules, set `effort_sizing = false` under the global
`[models]` and restart archond; new spawns then get 1.6.3's effort rules,
byte for byte. Current contracts ask stages with a verdict or required review
or ship records to call `submit_verdict` before finishing.
A running orchestrator keeps the prompt it was created with until
`archon orchestrator restart`; archond refreshes it by itself only after an
upgrade to another build. A repository config may still say `effort_sizing =
false`, which 1.6.3 accepted, but it changes nothing; `true` there is refused.
Each spawn records the rule it was issued, so in-flight spawns keep theirs
across the switch: a lead spawned before it, and one resumed, woken or
replayed after the restart, is held to its own rule, and a reviewer its rule
allows is never cancelled. Each spawn records the reviewers its contract
named as well, and the watchdog holds a lead's reviewers to those, not to the
item's plan as edited since. A spawn stored by 1.6.3 records neither: archond
reads its rule and its reviewers from that spawn's own reviewer and delegation
rule lines, never text the contract quotes. Where they tell nothing, it
cancels no reviewer for its effort, and only the model policy bounds the
reviewer's model. A model policy change the operator makes (`max_effort`,
allow or deny lists) still applies to running leads. Nothing here migrates the store, but going
back to a 1.6.3 binary is not supported once any release's store migration has
run: use the switch instead.

### Model catalog availability

Archon holds queued work with `model_catalog_unavailable` until Aether lists
a configured model. Start or reconnect Aether, or configure a provider when
its catalog is empty. Dispatch resumes automatically. Aether 1.4.1 supports
catalog checks. With retirement metadata, a refusal can also name a successor.
An enqueue that names only a tier also needs the catalog to resolve it, so
during an outage it is refused with the outage named, as 1.6.3 refused it. Retry
it once Aether lists models again. An explicit pin is accepted and checked at
dispatch, and a keyed replay returns its stored item.

After a store migration, the schema guard refuses to open the newer store
with an older daemon. Binary downgrade is unsupported. Use the documented
feature switches for rollback. The dispatch catalog check has no bypass.

### Efforts sized by risk class

With `effort_sizing` on, archond now sizes every effort a plan does not name
when it writes the plan ([configuration](configuration.md)): it classes the
item `mechanical`, `ordinary`, `elevated` or `critical` from its tags, title,
brief, review level and known diff size, and sets each unnamed effort from
that class and the stage's own default. An unnamed effort that 1.6.3 filled
with the stage default may therefore be `low` for documentation work (and, with
`sizer_max_effort` set, `high` for work that names a risk and `x_high` for
work tagged `risk:critical`). An
effort the plan names is kept as before. `get_item` and `archon queue show`
list the class and the reason for every entry, and each attempt records its
`risk_class`, `effort_source` and `effort_reasons`.

Effort is the orchestrator's choice: the sizer never changes an effort a plan
sets, and by default it only lowers default-effort stages of mechanical
items. `[models] sizer_max_effort` (unset by default, global only) opts into
raising default-effort stages of elevated and critical items up to that
effort; `max_effort` still caps everything.

The sizing is stored with the plan. Items queued or running across the
upgrade keep the plan they have: nothing is sized at recovery, and in a plan
1.6.3 wrote only an entry written afresh later (an inserted stage, a
`model_plan` a retry names) is sized. Changing
`effort_sizing` or `sizer_max_effort` and restarting affects only plans sized
from then on. One thing moves for a stored plan: at dispatch an effort the
sizer set above the policy's `max_effort` is lowered to it with a recorded
reason, where a pinned effort above it is refused as in 1.6.3.

To roll back, set `effort_sizing = false` under the global `[models]` and
restart archond: new plans are then filled exactly as 1.6.3 filled them, and
their attempts record no class. Stored plans keep the efforts they were sized
to. Nothing here migrates the store; going back to a 1.6.3 binary is still
not supported once any release's store migration has run, so use the switch.

### Plan origins

Each item now keeps `model_origins` beside `model_plan`: where every model
and effort of its plan came from, who chose it and when. `get_item` lists
them as `origin <stage>: model …; effort …` lines (`; tier …` for a tiered
entry).

The daemon may change a field again only when it filled that field itself:
its source is `learned`, `tier`, `fallback`, `sizer`, `migrated` or `escalation` and the
daemon chose it. Every other field is a pin, and so are these two:

- `legacy_unknown`: an item stored before origins were recorded. Nothing is
  backfilled; every field of it reads this way.
- `unattributed`: a field no writer accounted for. Stored origins that
  cannot be read, that do not match the plan's stages and entries, or that
  were written for another item make every field of that item read this
  way. Nothing is repaired field by field, and the item stays readable.
  It should never be counted: a non-zero count is a bug to report, not
  something to fix by hand.

A pinned tier is the one thing that still resolves. When dispatch starts a
run, a choice whose tier is pinned is resolved to a model of that tier, and
that model is recorded as `tier`, chosen by the daemon; a queued one waits
for provider quota like any tier. From then on a reroute, a quota move and
a restart move it only within that tier. A pinned model whose tier the
daemon chose, and any pinned model of a run already under way when archond
was upgraded, stays where it is. A restack fix of a legacy item stays
`legacy_unknown`; a refresh of one is a new item with origins of its own.

`archon status` counts the `unattributed` and the `legacy_unknown` fields of
unfinished items, each on its own line when it is not zero. An unfinished
item whose row cannot be read is counted on a third line instead of failing
the count. The parts of `archon status` that read queued, blocked and
running items themselves still fail on such a row, as they did before.

A tiered entry also records who asked for its tier: the caller when the
request named one, the daemon when the stage's default supplied it. A quota
move changes the model's origin only; the tier keeps who asked for it. A
request the orchestrator sends through the CLI is recorded as the
orchestrator's, not the operator's.

Downgrading the binary is not supported once a store migration of the
release has run, because an older schema refuses a newer store. Restore the
backup taken before the update instead.

### Store migration V24

The first start of an archond with migration V24 upgrades the store in place,
in one write transaction. V24 adds `outcomes.class_source` and
`outcomes.failure_class_163`. It then recomputes the failure class of every
stored failed run from that run's stage records (the timed-out,
infrastructure, budget and review rules of the [changelog](../CHANGELOG.md)),
and keeps the old class in `failure_class_163` for one release. Only the
class columns and fields change. A run whose stage records were pruned keeps
its old class, marked `legacy_163`. So does a run whose records V24 cannot
read, whose ending stage left no record, which recovery failed ("cannot resume
the run: …", after dropping the stage rows it could not read), or whose outcome
observed more runs of a stage than are still stored: it logs the run,
recomputes nothing, rescores none of its lesson uses and leaves an unreadable
outcome record as it was. A record it cannot read never stops the migration:
that covers a run, stage run, observed-stage or review-tally row SQLite holds
as something other than text, one that does not decode, a stage run naming no
stage or attempt 0, a stage run the store would not write (settled with no
finish time), a run or stage run whose row holds, in a column that repeats its
record (id, stage, attempt, slot, state, start, item, pipeline, whether it
ended) or in its agent claim, another value than the record's, an
observed stage with no attempts, and a tally in any shape archond does not store (an object
in which each fail names its reviewer's Aether agent id, `agt_` and a ULID, the
commit that reviewer read and its findings).
archond logs, per class, the old count, the new count and the rows left on the
1.6.3 class. When a recomputed class changes whether a run is scored, V24
rescores the run's lesson uses and holdout trials and recounts the lessons it
used. It preserves lesson state, confidence, and clusters. A lesson V24
cannot recount (its row, use, or epoch is unreadable, or its record is
invalid) keeps its stored counts, and archond logs the skip. Live scoring
and maintenance apply the configured retirement policy after startup. V24 also
stores, in each outcome whose item is still stored and whose run holds a
recorded reviewer verdict (a review stage's pass or fail, the pass or fail of
the review a stage's result carries, read as the gate reads it, or a
reviewer's fail in archond's review tally), the pull request it reviewed. Outcomes whose item was already pruned have no
pull request, so the model evidence report leaves them out of its pull request
counts.

V25 attributes spend to stage runs. V26 adds indexes for evidence snapshots.
V27 adds an index for near-duplicate enqueue detection. V28 records queue-reason
intervals and pull request event times. V29 adds the live design session
tables, and V30 adds follow-ups and references to design notes.

**Downgrading the binary is not supported after V25 has run.** The schema
check of an older archond refuses a newer store ("store schema version 30 is
newer than this archon supports (24); upgrade archon"), so no older daemon can
read the upgraded store. Before the update, stop archond and copy `archon.db`
and any `archon.db-wal`. Going back means stopping archond, installing the
older release and restoring that copy. Everything recorded since the copy is
lost. To roll back the behaviour of a release while keeping the store, use its
configuration switches, not an older binary.

## Contents

- [Effort sizing on by default](#effort-sizing-on-by-default)
- [Store migration V24](#store-migration-v24)
- [From 1.4.x to 1.5.0](#from-14x-to-150)
  - [What changes](#what-changes)
  - [Store migrations V19 to V23](#store-migrations-v19-to-v23)
  - [Remote access](#remote-access)
  - [What a paired device may do](#what-a-paired-device-may-do)
  - [Steps, in order](#steps-in-order)
  - [If the update was interrupted](#if-the-update-was-interrupted)
  - [Going back to 1.4.12](#going-back-to-1412)

## From 1.4.x to 1.5.0

### What changes

| | 1.4.12 | 1.5.0 |
| --- | --- | --- |
| Daemon protocol (archond with `archon`, `archon mcp`) | 23 | 24. The Aether app (`archon-web`) may speak 23 or 24; every other client must match exactly. |
| Aether protocol Archon asks for | 24, oldest accepted 17 | unchanged |
| Aether | 1.2.11 | 1.2.11 or later runs the queue. **Aether 1.3.0 or later** is needed for remote access: only it mounts archond at `/archon`. Archon 1.5.0 is released with Aether 1.3.0. |
| Store schema (`archon.db`) | 18 | 23, migrated at archond's first start |
| `remote.json` | format 1 | format 1 while every device is an operator; format 2 once an observer is paired |
| Remote access | archond served its own web app over `tailscale serve` or your proxy | the same app, now reached through Aether's listener at `https://<address>/archon/` (archond is mounted at `/archon`) |
| A hung orchestrator turn | archond nudged it and told you after 30 silent minutes; your messages waited until you pressed Esc | the same notice, after 10 minutes once your message waits, then archond interrupts the turn 5 minutes later, as Esc would, so your messages reach a fresh turn ([troubleshooting](troubleshooting.md#the-orchestrator-or-aether-is-down)) |
| Effort sizing | agents chose efforts by 1.4.x's rules; a reviewer plan's effort was a floor | the same by default; `[models] effort_sizing = true` opts in to sizing every effort by one rubric, a reviewer plan's effort then its default rather than a floor. On by default from 1.6.4: [Effort sizing on by default](#effort-sizing-on-by-default) |
| Default effort cap | no cap | `x_high`; set global `[models] max_effort = "max"` to allow existing `max` choices |
| `config.toml` | | new keys `[remote] mounted_at`, `[remote] trusted_origins` and `[learning] holdout_rate` |

A 1.5.0 `archon` and a 1.4.x archond refuse each other at `hello`, so
upgrade both together: `archon update --yes` restarts archond for you. A
stage agent's `archon mcp` that outlives the update restarts itself on the
new binary once archond refuses it, as since 1.4.2, so running stages carry
on.

With the new default cap, an orchestrator configured at `max` is refused, as
are stored plans and running children above the cap. Set global `[models]
max_effort = "max"` before upgrading if those choices must continue; repository
settings can only narrow the global cap.

### Store migrations V19 to V23

archond 1.5.0 upgrades a schema 18 store in place the first time it starts,
in one write transaction: either every step lands or none does.

| Version | Adds |
| --- | --- |
| V19 | `changes`, the gapless change stream, seeded with one change per item at its current state, and the item transitions behind `item_history`, which start empty. |
| V20 | `github_links`, backfilled from item origins (`origin` links) and recorded pull requests (`delivered` links). An origin number too large to store is skipped. |
| V21 | `github_objects`, the read-only cache of linked issues and pull requests. |
| V22 | `github_outbox`, which posts each GitHub comment at most once. |
| V23 | `lessons.cluster` and `lesson_trials`. No lesson row is rewritten. |

The migrations are forward-only. Archon 1.4.12 refuses a migrated store
("store schema version 23 is newer than this archon supports (18); upgrade
archon"), and archond takes **no copy of its own** before it migrates: back
the store up yourself first (step 2 below).

### Remote access

- `archon remote setup` now mounts archond on an Aether listener at
  `/archon` instead of publishing archond. It needs Aether 1.3.0 or later
  (your own Aether in shared mode, Archon's in dedicated mode), refuses while
  anything on the machine is published through Tailscale Funnel, withdraws
  a `tailscale serve` that published archond directly, publishes Aether's
  listener as the one origin, and asks for one `archon daemon restart`
  ([remote-setup.md](remote-setup.md)).
- `--proxy-url` and `--local` are removed. Open the phone app at
  `https://<address>/archon/` on the address `archon remote setup` prints;
  update the home-screen shortcut or bookmark to that address. A proxy that
  pointed at archond still finds the app at its listener, but a mounted
  archond accepts only Aether's origins, so use the mount.
- Setup writes two keys into `config.toml`, keeping the rest of the file:

  ```toml
  [remote]
  mounted_at = "/archon"                             # served behind Aether's mount
  trusted_origins = ["https://box.tail1234.ts.net"]  # Aether's public origins, at most 16
  ```

  archond reads them only when it starts. With them it scopes its device
  cookie to `/archon`, answers only loopback `Host` values on its own port,
  and accepts only these origins.
- Devices paired with 1.4.x stay valid, as operators, until
  `archon remote revoke` removes them. A browser that paired on the hostname
  Aether now publishes still sends its old cookie (scoped to `/`), and the
  mount accepts it; one that paired on another hostname must pair again with
  `archon remote pair`. Aether's own device is paired separately, with
  `archon aether remote pair`. Pairing again does not revoke the old device:
  revoke it to retire that access.

### What a paired device may do

- A device is an `operator` (the default, and every device paired before
  1.5.0) or an `observer`, paired with `archon remote pair --observer`,
  which only reads.
- **A device paired as an operator has your full operator power**: its
  requests run with the operator CLI's standing and its chat lines are
  yours, so from the app it does what you do from the TUI (`/archon`,
  `/answer`, `/approve`, `/merge`, directives, settings, the GitHub watcher,
  `merge=auto` within the ceiling, hard pause). Every device paired before
  1.5.0 is an operator: re-pair a phone others may hold as an observer, and
  revoke any device you lost.
- An observer only reads: the app's `say` and `answer` are refused to it
  with "… This is an observer device: it can only read. Pair it as an operator
  to act from it.". A
  device Aether paired without stamping its operator role (an older Aether)
  carries no operator authority
  ([vnext-audit.md](security/vnext-audit.md)): its chat is not operator
  chat, and its answer in the notification center is refused for Archon's
  gates and questions.
- Devices may also read the change stream, item history, the task graph,
  run plans and GitHub links, and operators may link and unlink issues
  and import one ([remote.md](remote.md#what-a-device-may-do)).

### Steps, in order

1. **Check Aether.** `aether --version`. To use remote access, upgrade
   Aether to 1.3.0 or later first, as its own upgrade notes say. The queue
   runs on Aether 1.2.11 as before.
2. **Back up.** Stop archond and copy the store and the files beside it:

   ```sh
   archon daemon stop
   cd ~/.local/share/archon      # or $ARCHON_DATA_DIR; ~/Library/Application Support/archon on macOS
   cp archon.db archon-1.4.db
   if [ -f archon.db-wal ]; then cp archon.db-wal archon-1.4.db-wal; fi
   if [ -f remote.json ]; then cp remote.json remote-1.4.json; fi
   ```

   A cleanly stopped archond leaves no `archon.db-wal`, and `remote.json`
   is absent if remote access was never configured; going back relies on
   knowing which of the two you copied. Keep a copy of `config.toml`
   (`~/.config/archon`, or `$ARCHON_CONFIG_DIR`) as well.
3. **Install 1.5.0.** `archon update --version v1.5.0 --yes` (or `archon
   update --yes` once it is the latest release).
4. **Start and check.** `archon daemon start` migrates the store. Then
   `archon --version` says `archon 1.5.0`, and `archon status` and `archon
   doctor` report the daemon healthy. `archon orchestrator restart` gives a
   running orchestrator the new tools if the update did not.
5. **Remote access, if you used it.** Run `archon remote setup`, then
   `archon daemon restart` when it asks, then `archon remote setup` again.
   A browser paired on the hostname Aether now publishes keeps working; pair
   one that paired on another hostname with the code setup shows (or
   `archon remote pair`), and with Aether through `archon aether remote
   pair` if it has no Aether device yet. Revoke the 1.4.x devices you no
   longer need with `archon remote revoke`.

### If the update was interrupted

A plain `archon update --yes` from 1.4.x installs 1.5.0 and then asks the
running archond to restart. Stopped in between (Ctrl+C, a closed terminal),
it leaves the 1.4.x archond running, and the installed 1.5.0 cannot reach
it: commands say "this archon speaks daemon protocol 24 but archond does
not", and `archon daemon stop` and `archon daemon restart` fail the same
way. Stop it by the pid it writes beside its socket, wait until it has
exited (a few seconds), then start 1.5.0's; running stage agents keep
running in Aether:

```sh
pid=$(cat "$XDG_RUNTIME_DIR/archon/archond.pid")
kill "$pid"
while kill -0 "$pid" 2>/dev/null; do sleep 1; done
archon daemon start
```

Without `$XDG_RUNTIME_DIR`, as on macOS, the socket and its `archond.pid`
are in the data directory; with `$ARCHON_SOCKET` set, the pid file is that
path ending in `.pid`.

### Going back to 1.4.12

Stop archond, install 1.4.12 and put the copies back. Everything 1.5.0
recorded since the copy is lost.

```sh
archon daemon stop
archon update --version v1.4.12
cd ~/.local/share/archon
rm -f archon.db-wal archon.db-shm
cp archon-1.4.db archon.db
if [ -f archon-1.4.db-wal ]; then cp archon-1.4.db-wal archon.db-wal; fi
if [ -f remote-1.4.json ]; then cp remote-1.4.json remote.json
elif [ -f remote.json ]; then mv remote.json remote-1.5.json; fi
```

A `remote.json` that 1.5.0 wrote where 1.4.12 had none is moved aside: its
devices were paired from the Aether app, and 1.4.12 refuses the format 2 an
observer brings. 1.4.12 also refuses `config.toml` keys it does
not know: remove `mounted_at` and `trusted_origins` under `[remote]` and
`holdout_rate` under `[learning]` (or restore your copy), then `archon daemon
start`. Aether's `/archon` mount points at archond's listener, which 1.4.12
serves as its own app: remove it with `archon aether remote mount remove
/archon`, then run 1.4.12's `archon remote setup` to publish archond again.
