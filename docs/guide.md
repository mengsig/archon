# User guide

Start with the [README](../README.md) for installation and a first pull request.
This guide keeps the detailed operating instructions. Paths below use Linux
defaults; see [configuration](configuration.md) for macOS and directory overrides.

## Runtime and model selection

Archon composes prompts and manages work; Aether runs the model turns.
Its native engines use Claude Code and Codex, and configured Aether
providers can also supply models. `archon aether models` shows availability.
If you are already signed in to Claude Code or Codex, neither Archon nor Aether needs a separate login for that engine.
The orchestrator chooses a model and effort for every stage, taking account
of complexity, risk, operator instructions and recorded cost evidence.
Pipeline nudges are advice, not enforced model choices. It records a short
`model_reasons` explanation and can query `model_evidence`. You predefine
no models or tiers: every model Aether reports ready may be chosen unless
`[models]` in the config narrows that. The daemon checks each choice
against that policy, the caps and the budgets.

The SQLite store preserves the queue and run state. Work-creating Aether
requests use idempotency keys so replay after a restart does not create a
second agent for the same request. Archon uses Aether's public protocol.
By default its dedicated Aether instance keeps Archon's tools separate from
your ordinary Aether instance.

## How it works

```
you ─ archon chat ─▶ archond ──▶ Aether ──▶ orchestrator agent
                        ▲                       │ archon MCP tools
                        └───────────────────────┘ (enqueue, notify, ask_operator, …)
                        │
                        ├─ backlog, caps, budgets, model policy (SQLite)
                        └─ pipeline runs ──▶ stage agents in per-item git worktrees
```

1. You tell the orchestrator what you want: in the terminal UI (`archon`),
   in the line chat (`archon chat`) or with `archon say`.
2. The orchestrator decides how the request is done, taking the first of
   these that fits: answer it directly, reading your code where that takes
   it, hand it to a helper agent, or queue it through a pipeline, a large
   request as a backlog of items.
3. For queued work it calls `enqueue` once per unit of work, choosing a
   repository, the pipeline that fits and a model and effort for each stage.
4. `archond` dispatches items as capacity allows. It creates a git worktree
   for each item and runs the pipeline's stages as Aether agents.
5. The pipeline enforces its own review and test gates and fix loops.
   `archond` posts every outcome and blocker to your feed itself, and wakes
   the orchestrator only for those it has something to do about.

### What the orchestrator does without queueing

Answering needs no item, and neither does reading your code. The
orchestrator answers what it already knows. For a question about a
repository it reads the code itself and answers in the same conversation.
For research on the web or in documents, a wide read, a comparison, a
draft or a second look at a result it delegates to a helper agent (Aether's
`delegate`). No worktree is made and nothing is shipped.

It reads code in a mirror: for every registered repository `archond` keeps
a clone at `~/.local/share/archon/orchestrator/repos/<name>`, checked out
at the base branch. A mirror holds committed work only and has no remote,
so nothing done in it reaches your checkout, your branches or GitHub.
`archond` refreshes it when an item finishes and when you write to the
orchestrator, and says so in the feed if a refresh fails; the orchestrator
is told as well, so it knows when what it reads may be behind. The `ask`
pipeline is still there for a question you want answered as a recorded
item, but nothing needs it.

Every change to a repository is an item, so the pipeline's
gates, your ship mode, caps and autonomy ceiling apply. The orchestrator
picks the cheapest pipeline that fits: `direct` for any change one author
can make and check, whatever its size, where one agent does all of it,
`adaptive` where one lead that keeps its context through fixes or
integrates independent parts saves restarts, and a heavier one only when
the work clearly calls for it. How much review an item gets is a choice of
its own, set by risk rather than size: see
[Autonomy levels](#autonomy-levels). The orchestrator adds no review on top
of a pipeline, and when work was split into parts it has the integrated
result reviewed once, with a depth that follows the risk.

What to know about helper agents:

- A helper works in one mirror, or in the orchestrator's scratch directory
  when it reads no code, and never in your checkout. To build or try
  something it gets its own copy of the mirror (Aether's isolated
  workspace). `archond` cancels a helper that Aether placed in a registered
  repository or among the items' worktrees, also when the path only leads
  there through a symlink, and posts a blocker. This checks where the
  helper was placed; what a native agent's tools can reach beyond that is
  up to Aether
  ([`docs/ARCHITECTURE.md` §17](ARCHITECTURE.md#17-aether-limitations-and-how-archon-handles-them)).
- A helper must use a model and effort that `[models]` allows and that the
  repository whose mirror it reads does not refuse in its own `[models]`.
  Another repository's policy does not apply to it. One outside is
  cancelled. Its spend counts toward your daily and total budgets. Helpers
  are not counted by `max_parallel_agents`; the orchestrator is told to
  stay within that cap, and the daemon does not enforce it.
- A helper may hand part of its work to a helper of its own, once.
  `archond` cancels an agent a third delegation below the orchestrator.
- Helpers cannot queue, cancel or change anything in Archon: those tools
  answer only the current orchestrator (see
  [After upgrading](#after-upgrading) for what that depends on).
- Helpers belong to the orchestrator agent. `archon orchestrator restart`,
  and any recovery that replaces the orchestrator, cancels them. A cancel
  Aether could not take is kept and retried until Aether confirms it.
- A GitHub request without operator standing is never delegated: the
  orchestrator answers it itself, from that repository's mirror when it
  takes code. Its prompt says so; `archond` cannot tell which request a
  helper serves and does not enforce it.
- A helper's result reaches you through the orchestrator: in its reply to
  the message you sent when it waited for the helper, otherwise as a
  notification. Such a reply is two lines in the chat: what the orchestrator
  said before it waited, then the answer.
- If the orchestrator has no `delegate`, or Aether refuses a delegation, it
  says why and uses the `ask` or `errand` pipeline instead.

### Pipelines the orchestrator writes

When you keep asking for the same shape of work and no pipeline captures it,
the orchestrator writes one into your user pipelines directory
(`~/.config/archon/pipelines/`), checks it with `archon pipelines check` and
tells you its name. It never overwrites a pipeline it did not write. A
pipeline inside a repository (`<repo>/.archon/pipelines/`) is committed with
the repository, so there it only proposes the pipeline and waits for you.

Preferences that should outlast a conversation are kept in two places: a
standing instruction becomes a directive (`archon autonomy` lists them),
which every future orchestrator reads, and what it learned about how you
work goes to Aether's memory.

### After upgrading

A running orchestrator keeps the prompt, tools and workspace it was created
with, also across Aether's `restart`. Run `archon orchestrator restart` once
after upgrading so that a new orchestrator starts with the current ones.

Archon's tools that change something answer only the current orchestrator,
and `archond` knows who is calling only when Aether names the agent behind
each call: `send_caller_meta = true` on the `archon` MCP server in Aether's
config. Aether reads that setting only as its daemon starts.

- **Dedicated instance (the default).** Nothing to do. The config is
  Archon's own: the next `archon` command that needs Aether brings the
  registration up to date and restarts Archon's `aetherd` so that it takes
  effect. That restart happens once, and interrupts the agents then running
  in that instance as any Aether restart does; Archon recovers its items.
- **`--shared`.** Archon never edits your Aether config. `archon doctor`
  fails with the line to add; add it and restart your `aetherd`, then run
  `archon orchestrator restart`.

Until Aether names the callers, `archond` posts one blocker saying so,
accepts those tools only from an orchestrator that cannot delegate (so the
caller can be nobody else), starts new orchestrators without native
delegation, and refuses every other unnamed call. An orchestrator from
before the upgrade cannot delegate and keeps working. One started with
`delegate` before `archond` saw the first unnamed call has that call
refused: `archon orchestrator restart` replaces it with one that works
until you have made the fix above. Once the orchestrator makes a named
call, a read included, `archond` says so and the next orchestrator
delegates again.

## What Archon learns

Archon keeps outcomes and short lessons in its own store. `archon lessons stats`
shows per-repository pipeline pass rates, correction rates, median cost and wall
time, and learned model defaults with sample sizes. The existing
`model_evidence` tool includes compact pipeline evidence for the
orchestrator's pipeline selection. These are observations, not guarantees.
Estimated model savings compare median stage costs with the static default a
plan would have had without learning, need at least five samples of that
default, and are shown per planned run. Missing baseline evidence is reported
as unknown.

```sh
archon lessons --repo my-repo
archon lessons --all
archon lessons add --repo my-repo --kind pitfall "Run migrations against an old database."
archon lessons edit lsn_ID "Check both upgrades and fresh databases."
archon lessons pin lsn_ID
archon lessons retract lsn_ID
archon lessons accept lsn_ID
archon lessons why qi_ID
archon lessons stats --repo my-repo --json
archon pipelines proposals
archon pipelines proposals learned-check
archon pipelines accept learned-check
```

`why` shows the frozen lesson text and routing sample sizes used for an item.
A run uses at most six lessons, with a 1,200-byte rendered section, preferring
its repository and pipeline. Lessons are untrusted advice and cannot expand
permissions. Duplicate text is merged; operator edits take precedence. Wins
are successful runs without corrections. Five or more scored uses can retract
a lesson whose raw Wilson lower bound drops below 0.3. Confidence decays since
last use; age alone never retracts a winning lesson. Pinning prevents
automatic retraction and decay; `pin --unpin` releases the pin.

Ordinary chat and scope or effort steers never count as model failures. To record
an explicit correction, send a structured operator message:
`correction {"item":"qi_ID","stage":"implement","cause":"quality","reason":"The result missed the invariant"}`.
Causes can also be `scope`, `infrastructure` or `budget`; only quality feedback
penalizes the named stage. A reviewer that correctly finds a builder's error is not
penalized. Run-level retry, gate and GitHub observations remain visible as audit
history and can revoke lesson wins.

The orchestrator's lessons require a source item and stay scoped to its repository.
They begin Proposed with low weight until a confirming win or your acceptance.
Distilled proposals wait for `archon lessons accept`. Lessons are one plain sentence
without markup, links, external paths or instruction overrides. Use `lessons --all`
to inspect proposed advice. Read-only lessons commands also work with the daemon
stopped and do not start Aether.

Routing explores ready, permitted models on at most ten percent of item IDs,
including models with no history. `why` shows Wilson quality intervals, cost and the
pipeline revision, review level, task class and diff-size bucket. You can tag an item
`task:maintenance` or `diff:small` to keep evidence relevant. Costs may settle after
completion; statistics and pending completion news use the current ledger figure.
Distillation has one errand per repository at a time and a global default $3 daily
reservation. Imported history is not distilled. See [configuration](configuration.md#learning).

### Correcting one stage with `archon learning`

```sh
archon learning correct qi_ID --run prun_ID --stage implement \
  --cause quality --reason "Ignored the current Git identity rule" \
  --forget-memory mem_ID
archon learning why qi_ID
archon learning undo fb_ID
archon learning restore-memory fb_ID
archon learning retry-memory fb_ID
```

`correct` records what went wrong in one stage of one finished run of the
item. Archon checks that the run belongs to the item and ran that stage, then
freezes the repository, pipeline, routing context (pipeline revision, review
level, task class), the stage run, its model and effort, and the delivered
commit while Archon still keeps it. Only the operator's `archon` command can
correct or curate: an agent is refused, through `archon mcp` or any other
client that forwards its request.

A `quality` correction counts against a model only when one model choice
plainly produced the stage: a stage that ran as a work stage, by the kind
Archon recorded when it ran, of a finished run that was not reshaped, run by
one agent on one model, whose last attempt completed and whose outcome holds
a successful routing sample for it. Anything else, such as a fan-out, a model
switch mid-stage, a review stage or a stage that ran before Archon recorded
its kind, is recorded as an observation, and the receipt says why there is
no model penalty. `scope`, `infrastructure` and `budget` corrections are
informational.

The corrected run's successful sample of that stage and choice then counts as
unsuccessful, once, however often the same correction is sent. Only new
learned defaults in the same repository and routing context change: explicit
choices and plans already chosen keep theirs. A repository cohort with a
correction never borrows global evidence, reads only runs that finished in
the last 90 days, and explores no other model while a correction is active.
A penalty lasts 90 days. Review duties, gates, CI and shipping checks are
unchanged.

`why` lists each correction of an item's runs: whether it is active, revoked,
expired or informational; the corrected choice's results now; what a new
default selects; the routing explanation kept when the item was planned; and
the named memory's origin, scope, date and state. It also shows sent, pending,
skipped and retracted memory feedback, partial context results and failure
reasons. `undo` revokes the model penalty immediately and queues retraction of
memory feedback; the record stays. A memory the correction forgot stays
forgotten.

When Aether supports memory feedback, quality corrections send outcome evidence
for the memories recalled by the corrected stage's tasks. Other causes send no
outcome evidence. A named `--forget-memory` sends the operator's judgement of
that memory for every cause, provided it appeared in the task's context.
Archon records this delivery intent durably, then delivers it in the background;
a stalled feedback call does not delay `correct` or `undo`. Pending work resumes
at daemon start and when Aether reconnects. Replaying a correction or running
`retry-memory` also resumes delivery. Permanent refusals, missing features and
instance mismatches are skipped with their reason in `why`.

An interrupted submission is resolved before any resend. Aether's retained
receipt recovers the feedback id for undo; a proven safe absence permits a
resend. If Aether cannot tell whether it ran, including after receipt expiry,
`why` shows delivery unconfirmed and Archon leaves it pending without resending.
An older Aether that cannot resolve submissions also leaves uncertain work
pending. `retry-memory` can check for a receipt again; it never forces an
uncertain resend.

`--forget-memory` forgets that one Aether memory in the Aether instance
`archond` last connected to, so fresh stage sessions stop recalling it;
sessions already running keep what they recalled. Archon stores the memory's
ID and state, never its text, and shows who wrote it as Aether recorded it.
The correction is recorded first: when Aether is down or the forget fails,
the command prints "Correction recorded; memory forget pending/failed" and
exits 1, and `retry-memory` retries the forget and pending feedback in their bound
instance. The explicit forget still waits on Aether and can time out; background
feedback does not delay it. A forget is refused before anything is recorded when
`archond` has never connected to Aether or is switching to another instance, and
when `aetherd` is older than protocol 15. `restore-memory` brings back a memory
this correction forgot; it
refuses when the memory was already forgotten or superseded before the
correction, has been restored or forgotten again since, or is superseded now.
Aether's `/memory` (`u`) restores any memory.

With `[learning] enabled = false`, `correct` and `undo` are refused and
nothing is recorded; `restore-memory` and `retry-memory` still work.

## Agents talking to the orchestrator

Every stage agent can call Archon's `message_orchestrator(text, kind)` in
guarded and yolo modes. Use `relay` to pass on an operator message sent to
the wrong agent, `escalation` for a blocker outside the stage, or `info`
(the default). Text must be non-blank and at most 4096 characters. The
tool returns a receipt immediately; the agent must continue its stage
without waiting for a reply or ending its turn.

Archon writes the message to the feed itself, whole, with its stage, sender
and kind (`archon log --item <id>`): an escalation shows in the chat as a
blocker, a relay or info with progress. It also delivers an `agent_message`
news event to the orchestrator, which acts on it and does not repeat it to
you. Delivery is durable and coalesced; each
agent can send at most 20 messages (`[limits] max_agent_messages`), with
a clear refusal after that.
Delivered messages to passive run nodes are forwarded through the same
path, including late events for retired nodes whose normal index was pruned. External
messages have a separate cap of the same size per run node; refusals reach the
sending agent or the operator feed. Use the tool for stage messages.

Relay wording alone grants no operator authority. A relay has operator
provenance only for an authenticated operator inbox sender, or when a
stage quotes exactly the latest operator message Archon observed in its
inbox. Other relays retain agent provenance. The orchestrator can answer
using Aether `send_message`, a retry with `brief_append`, or new items.

`message_orchestrator` needs Aether to name its caller
(`send_caller_meta = true`, see [After upgrading](#after-upgrading)): a call
without caller identity is refused. In dedicated mode Archon keeps its
registration current by itself. For an existing shared registration, add
that field to the `archon` MCP server's `[[mcp.servers]]` entry in the
active Aether config, add `message_orchestrator = "mutation"` to its
`classes` table, and restart that Aether daemon. `archon doctor` reports a
registration that lacks the field in both modes. Setup preserves existing
entries.

A registration written before `record_chat_answer` existed does not list
that tool, and Aether then classes it as `external`. Run
`archon setup --resync` in dedicated mode, or add
`record_chat_answer = "mutation"` to the shared registration's `classes`
table. The `/answer` chat command does not depend on it.

## Pipelines

Each pipeline is a directory with a `pipeline.toml` and one Markdown prompt
per stage. Add your own under `~/.config/archon/pipelines/` or
`<repo>/.archon/pipelines/`. See [`docs/PIPELINES.md`](PIPELINES.md).

| Pipeline | Stages | Leans toward |
| --- | --- | --- |
| `direct` | implement | balanced models; the default, and the cheapest path for a change one author can make and check, whatever its size: a fix, a feature, tests, docs or a dependency upgrade. One agent implements it, runs the affected checks and commits, and Archon ships. No review unless the item's `review` knob asks for one; the agent hands back work that turns out risky. See [`docs/PIPELINES.md`](PIPELINES.md#direct) |
| `adaptive` | build (delegation enabled, holds the review) | balanced models; the orchestrator's choice where one lead that keeps its context through fixes, or integrates independent parts, saves restarts. One lead builds it, delegates the parts worth splitting and has the integrated diff reviewed by as many independent reviewers as the item's `review` knob asks for (one by default); Archon ships only a build with those reviewers verified. Chosen by name or as a repository's `default_pipeline`; see [`docs/PIPELINES.md`](PIPELINES.md#adaptive) |
| `refactor` | baseline/characterize → refactor → verify (gate, back to refactor ×3) → review (gate, back to refactor ×2) | balanced tier, independent reviewer; structure-only changes, use `direct` for behaviour changes |
| `performance` | measure → optimize → verify (gate, back to optimize ×3) → review (gate, back to optimize ×2) | strong tier, independent reviewer. For a specific slow path with a measurable goal; use `direct` for ordinary changes. |
| `agent-testing` | plan → exercise (fan-out) → report | agents using the product as a user would |
| `code-audit` | scope → audit (3 independent lenses) → report (HTML with issue drafts) | code inspection with evidence and follow-up findings; use `ask` for one question |
| `e2e-testing` | setup → e2e (gate) → report | HTML and screenshot reports for Lavish |
| `ask` | answer | a read-only answer about a repository, with no PR |
| `errand` | errand (report) | a small job that changes no repository, answered in text or as a Lavish page; no outward side effects |
| `design-doc` | research → critique (gate, retry ×2) → report (HTML design/RFC) | design before a large or ambiguous change, strong tier and independent critic; no product-code changes or shipping. Use `proposal: lavish` for design and implementation in one item |

A change that splits cleanly into a backend and a frontend part can run the
[`team-pr` example](examples/pipelines/team-pr/pipeline.toml): copy its
directory to your pipelines and edit its roles.

A work item ships in one of two modes, set per repository:

- `pr` (the default): opens a PR. Merging requires the item's `merge = auto`
  knob and permission in your ceiling (`--auto-merge` permits it for a repository),
  or your own request ([Merging on request](#merging-on-request)).
- `local`: commits on the run branch and never pushes.

No agent ships. A stage of the pipeline, usually the last that commits,
returns what is to ship (the commit, its tree, the checks it ran, the PR
title and body), and `archond` delivers it in a few seconds: it checks that
the worktree is clean, still holds that tree and carries only commits under
your repository's git identity (as `archon repo add` recorded it; run
`archon repo set <repo>` after you change it), pushes without ever forcing,
and opens the PR with `gh`, or reuses the one already open for the branch. A
delivery that is refused fails the item with what to change, and a restart
in the middle never opens a second PR. The item succeeds once its PR is
open: `archond` does not wait for your CI and does not repair a failing
check. Ask for a fix, or run `archon queue retry <id> --append "…"`, which
delivers into the same PR. See [Shipping](PIPELINES.md#shipping).

## Aether setup

`archon setup` creates Archon's own Aether instance in
`~/.config/archon/aether/` on Linux (under
`~/Library/Application Support/archon/aether/` on macOS). Its `config.toml` is a copy of your
`~/.config/aether/config.toml` (engines, providers, network and approval
settings) plus the `archon` MCP server. Your `providers.toml` and TUI
preferences and optional custom `themes/` directory are linked, not copied. `voice.toml` is the exception: the
instance gets its own copy, which `/voice setup` in `archon tui` then
writes, so a later change to yours does not reach it. `archon setup` never
writes to the operator's Aether configuration in dedicated mode. After you change your Aether config, run
`archon setup --resync`. `archon doctor` checks everything.


## Use

```sh
archon        # the terminal UI: calm Aether TUI on Archon's main orchestrator
archon tui    # the same by name; it takes the options below
archon chat   # the quiet line chat, for SSH, scripts and pipes
aether tui    # plain Aether, untouched
```

A bare `archon` opens the terminal UI with default options, as `archon tui`
does. It opens the line chat instead, and says why in one line on standard
error, in two cases: standard input or output is not a terminal, or
`[aether] address` (TCP) is configured, which the terminal UI cannot use. Any
other failure to open the terminal UI (Aether missing or too old, no
orchestrator to attach to) is an error, not a fallback; it names
`archon chat` and `archon doctor`.

`archon tui` starts
Archon if needed, asks its supervisor for the live orchestrator, then replaces
itself with `aether tui --agent <id> --view-mode calm` in dedicated mode. A healthy orchestrator is reused;
restarts made in Aether are followed. Attachment sends no user message and
respects revival backoff. An unavailable attachment stops with a visible error;
it never deliberately opens a generic root. If the agent disappears after
launch, Aether may select another root: exit and run `archon tui` again.

In shared mode, `archon tui` uses `[aether] socket` ahead of `AETHER_SOCKET`;
without a configured socket, Aether's usual environment/default resolution
applies. A configured `[aether] address` (TCP) is rejected before attachment
because the stock TUI requires a Unix socket. Raw `archon aether` passthrough
keeps Aether's own endpoint selection.

The native board shows new stage agents as `#q69g5fav implement · Chat polish`,
with fan slots when needed and attempt/retry notes only above one. Run nodes
use the item title; the default orchestrator name is `Archon`. Configured names
and existing agents keep their names. Aether owns state colours, layout,
motion, history, multiline input and native approvals. Archon adds no renderer
or refresh timer. `archon status`, `queue` and `log` retain their output.

Use `archon tui --theme nord --ascii --reduced-motion` to override Aether's
preferences for a run, or `--plain` for its line transcript. Environment and
`ui.toml` preferences remain Aether's: explicit theme / `AETHER_THEME` wins
over non-empty `NO_COLOR`, then the saved theme. Redirected stdout uses native
plain mode. ASCII changes UI glyphs, not message text. Archon's `--json` is
refused before launch.

Archon installs `modes/calm.toml` in its own Aether config directory on setup
and on TUI launch (requires an Aether with declarative view modes).
Existing definitions, including operator edits, survive setup and `--resync`;
setup reports seeded/unchanged and doctor validates edited definitions.
If calm cannot be seeded or loaded, the TUI warns and launches in standard
mode. Aether also shows mode diagnostics in `/mode`.

Calm applies only to role `archon-orchestrator`: compact delegation, inter-agent
messages, completions and status rows, plus full replies and decisions. Routine
tools, thinking, memory, compaction and usage cells are hidden. Questions,
approvals and failures stay visible. Ctrl+T reveals details; `/calm` toggles to
standard and back, `/mode standard` leaves calm, and `archon tui --normal` starts
in standard. Launch selection is session-only; each dedicated launch starts calm
unless `--normal` is given, regardless of saved or environment mode preferences.
Stage agents use the normal transcript when selected; the board shows the whole
fleet, including stage starts/finishes. Modes have no combined subtree feed.
Every Archon news payload carries a one-line `summary` of at most 160 characters,
naming the event and item (or the events in a batch). With Aether's trigger
summary support, calm shows `Trigger from archon · <summary>`
instead of a truncated JSON prefix; Ctrl+T reveals the full JSON payload.
Older Aether versions keep their existing trigger rendering.

Every new orchestrator generation first receives an `orchestrator_briefing`
built by the daemon from its store, without a model call. It lists unfinished
items with their stages, pull requests and waits, outcomes since the last
batch, open questions and gates with their Q numbers, open initiatives, and
up to eight recent chat feed entries. The briefing event is at most 4 KB;
`+N more` counts show omitted entries. Normal news follows it, and retries
reuse the saved briefing. If the agent stops before accepting it, the daemon
cancels its remaining children and retries with a fresh generation. Existing
generations keep their conversations.

The orchestrator is instructed to echo every stage→orchestrator `agent_message`
with stage/sender, kind, item and full message text, including info and coalesced
batches. These assistant replies keep incoming communication readable with
details off; they appear when the orchestrator processes the trigger, rather
than at delivery. This presentation depends on the orchestrator following its
prompt; the structured payload and authenticated provenance remain authoritative.
Archon enqueue and
notification tools are routine tools; their operator-facing text is conveyed by
the orchestrator's concise replies, not custom tool rendering. Shared mode keeps
Aether's own mode selection and never installs files in its config directory;
install the definition there yourself to use `/calm`, or use built-in `/focus`.

The standard transcript includes tools and trigger turns. Direct TUI messages
live in Aether's history; they do not add an Archon user-feed entry. Archon's
pipeline gates and notifications remain in Lavish / `archon questions` /
`archon answer` and the quiet chat; they are separate from native Aether
approval cards. A gate or question can also be answered from this chat with
`/answer <n> …` (see [Answering in the orchestrator's chat](#answering-in-the-orchestrators-chat)).
Existing orchestrators receive updated built-in instructions
on their next recreation (`archon orchestrator restart`); opening the TUI
does not force a restart.

### What the terminal UI shows, and what it does not

The terminal UI shows the orchestrator's transcript. The line chat shows
Archon's feed. They overlap, but they are not the same list.

- **Always shown:** every reply the orchestrator writes (also in turns woken
  by Archon news, which the line chat never shows), your own messages,
  Aether's native questions and approvals, delegations, stage messages,
  failures, and one row per Archon news trigger, such as
  `Trigger from archon · item finished: qi_… failed — <title>`.
- **Shown later than in the line chat, with `wake_on_success = false`:** an
  item that simply succeeded. The orchestrator is then not woken for it (see
  [When the orchestrator is woken](#when-the-orchestrator-is-woken)), so the
  terminal UI shows it with the orchestrator's next turn, as a row
  `3 routine items finished`, while `archon chat`, `archon log` and
  `archon status` show it as it happens.
- **Shown because the orchestrator restates them** (its instructions say so;
  the tool calls themselves are hidden in calm): the text of each `notify`
  and `ask_operator`, a published report's title and location, an item's
  outcome with its pull request URL, and each proposal or report gate when
  it opens: what is asked, a short summary, its number and the
  `/answer <n> …` line that answers it. Ctrl+T shows the calls and payloads.
- **Not shown:** the rest of what archond posts to the feed itself. That is
  "reopen the review window?" questions, reports published by stages,
  progress notices, system notices, and the chat's "open questions" header.
  A gate also opens in Lavish when it is installed; `archon status` and
  `archon questions` always show what waits.

### When the orchestrator is woken

Every turn of the orchestrator reads its whole context again, so a wake
costs money whether or not there is anything to decide. `archond` posts
every outcome to the feed itself and wakes the orchestrator for:

- an item that succeeded, so the terminal UI shows it: Aether's chat shows
  only what the orchestrator says, and Aether has no way to add an entry to
  it without a turn. While other items are still queued or running, a
  routine success waits for them, up to five minutes, so one turn reports
  the batch;
- an item that failed or is blocked, to diagnose it, retry it or tell you;
- an item tagged `notify` (`archon queue add --tag notify`, or the
  orchestrator's own choice when it wants to act on the result);
- an item a GitHub thread asked for, to post the result there;
- a stage agent's message, a gate that opened, your answer to its question,
  feedback on a report, a budget that was reached, a cancelled helper.

A cancelled item waits. So does a routine success when you turn success
wakes off, which saves a turn per item when you follow Archon in
`archon chat` or `archon status` rather than in the terminal UI:

```toml
[orchestrator]
wake_on_success = false
```

The orchestrator reads all waiting items as one short list at its next
turn, and a message from you delivers that list first, so it never answers
you without it.

### Answering in the orchestrator's chat

A message to the orchestrator that is exactly

```text
/answer <question number or q_… id> <answer>
```

is applied by archond itself, through the same path as `archon answer`: the
same validation, the same effect, and the same feed record, marked
`answered from operator chat`. Leading and trailing whitespace is allowed;
anything else in the message makes it ordinary text. For a proposal or
report gate the answer is `approve` (a note may follow; `yes`, `ok` and
`lgtm` approve too), `reject` to cancel the item, or any other text, which is
feedback that sends the work back. For a question with options, a bare option
number picks that option.

- The orchestrator still receives the message. It acknowledges in one line
  and then restates what archond did. A malformed command, an unknown or
  already answered number, or an answer that was refused comes back with the
  reason, in the same chat and as a system notice in `archon log`.
- Only your own message counts: one Aether authenticated as the `user`
  actor, or a GitHub request from a listed operator login
  ([Operator authority](operator-authority.md)). An agent's message, a
  relay, any other GitHub request, and a `/answer` quoted inside other text
  answer nothing. The orchestrator has no tool that
  answers a gate.
- A plain reply to a question the orchestrator asked itself (`ask_operator`)
  works as before, and the orchestrator now records it, so the question
  leaves `archon questions`, marked
  `answered in chat, recorded by the orchestrator`. It can do that only for
  a question it asked itself, never for a gate: archond stores which agent
  asked each question and refuses any other, including one another agent
  asked, one asked before `archon orchestrator restart` replaced the
  orchestrator, and one asked before this version. This depends on the
  orchestrator following its instructions; `/answer <n> …` or
  `archon answer <n> …` always closes it.
- With `[aether] mode = "shared"` chat answers are **off**: in a shared
  Aether instance a client that connects with the `[listen]` token is the
  same `user` actor as you, so a chat command would widen who can pass a
  gate. `/answer` is then refused with that reason; use `archon answer`.

Known limitations:

- A command is applied at most once. If archond stops in the instant between
  reading the message and applying it, nothing is answered and nothing is
  reported; the question stays in `archon questions`, so send it again.
- A running orchestrator learns about `/answer` and gate news from its
  instructions, which it gets on its next recreation
  (`archon orchestrator restart`). The daemon applies `/answer` either way.
- A question asked before you opened the terminal UI is in the transcript
  above, as long as the orchestrator that asked is still the one running; an
  orchestrator created since starts an empty transcript. `archon questions`
  always lists every open question.

### Upgrading and watching from the orchestrator's chat

Two more whole-message commands are applied by archond as `/answer` is, with
the same rules: only your own message (the `user` actor), only alone in the
message, and off in shared mode.

```text
/upgrade archon|aether|both <version|latest|main> [requeue]
/upgrade restart [requeue] [keep-aether]
/watch <registered repository>
```

`/upgrade` is `archon daemon upgrade` (or `archon daemon restart`): archond
holds dispatch while running stage agents finish (at most 30 minutes),
installs what you named while dispatch stays held, then hands the restart to a detached
`archon daemon relaunch` that outlives it, stops, and is started again with
Archon's own aetherd. The next archond reports the old and new versions in
the feed. Without `requeue` a drain that runs out gives the restart up and
dispatch goes on; with it the restart goes ahead and the stages it stopped
are retried. `keep-aether` restarts archond alone, so stage agents keep
running. A binary in cargo's bin directory is rebuilt with
`cargo install --locked --git … --tag v<version>` (or `--branch main`); an
installer's binary runs its own `<program> update`. `archon status` shows
the upgrade's progress, and running against installed versions when they
differ; `archon status --check-updates` also asks for the latest releases.

`/watch` is `archon github-watcher add` for a registered repository's
GitHub `origin`, so `github_reply` and GitHub triggers work for it. Agents
cannot do either: the orchestrator's `register_repo` never watches, and
both commands are refused for anyone but you.

`archon` starts Archon's `aetherd` and `archond` when they are not running,
as any command that changes something does; a command that only reads never
starts either ([which commands start archond](#which-commands-start-archond)).
`archon aether <args…>` runs any `aether` command against Archon's instance,
for example `archon aether agents`. `archon daemon stop` stops both
daemons (`--keep-aether` stops only archond). In the TUI's tree each work item is one subtree: a node named after
the item, with its stage agents (`implement · <title>`, `review 1/2 · <title>`,
`ship · <title>`) underneath, beside the `archon` orchestrator.

```sh
archon repo add ~/code/api --ship pr
archon repo add ~/code/web --pipeline adaptive
```

In `archon chat`, just talk. Slash commands cover the rest: `/questions`,
`/a [n] <answer>` (the newest open question without `n`),
`/answer <n> <answer>`, `/status`, `/queue [state …]` (the queue, e.g.
`/queue running failed`), `/item <id>` (one item), `/merge <id>
[--ignore-checks]` (merge its pull request; see
[Merging on request](#merging-on-request)), `/progress` (show stage
progress too; also `/verbose`), `/pause` and `/resume` (optionally `hard`,
`repo <name>` or `pipeline <name>`), `/help` and `/quit`. Start a line with
`//` to send a message that begins with `/`.

| Command | What it does |
| --- | --- |
| `archon` / `archon tui [options]` | the terminal UI on the orchestrator; a bare `archon` opens the line chat instead off a terminal or with `[aether] address` set |
| `archon chat [--progress]` | the quiet line chat with the orchestrator |
| `archon say "…"` | send one message to the orchestrator and exit |
| `archon questions` / `archon answer <n> "…"` | list or answer open questions; `Q<n>` is permanent and the same in the chat |
| `archon status` | waiting items and their next action, daemon, orchestrator, `archon` MCP server health, caps, spend and Aether link |
| `archon cost [--since <date\|7d>] [--until <date\|1d>] [--by day\|repo\|pipeline\|model\|item\|stage]` | spend as the budgets count it, grouped (by day over the last 7 days by default), with a total; spend with no owner in the grouping, such as the orchestrator's under `--by model`, is the `-` row |
| `archon model-evidence [--repo <name\|id>] [--pipeline <name>] [--stage <id>] [--since <date\|30d>]` | recorded stage attempts, completed cost median/p90, median minutes and review/test pass shares; small samples marked, at most 60 groups; evidence only |
| `archon queue add --title … --brief-file <path\|-> [--model-plan-file <json\|toml>]` | enqueue scripted work through the daemon's MCP-equivalent policy checks; stages the plan leaves out get Archon's default model; prints the id or the item with `--json` |
| `archon queue wait <id>… [--timeout 30m]` | wait for every item to become terminal, bounded to 30m by default; exit 0 all succeeded, 3 failed/cancelled, 4 timeout, 5 failed/cancelled dependency |
| `archon queue list` | one line per item: state, current stage, age, cost, title, PR URL and derived waiting reason with one next action |
| `archon queue show <id> [--full]` | an item's runs and each stage's attempt, slot, state, verdict, model, agent, cost and summary (first lines; `--full` for all), with the PR URL, recorded plan reasons and full waiting explanation when present. A stage that delegated shows how its cost splits (its own agent, then each delegated agent with model, effort and cost), and a stage holding a declared review shows the review on record. Steers made to the item are listed with who made them |
| `archon queue cancel <id>` / `archon queue caps` | cancel an item, or show and change saved operator caps |
| `archon queue retry <id> [--fresh] [--append "…"]` | queue a finished item again in its worktree and branch (a succeeded one delivers into its pull request again, and the retry counts against the run that passed); `--fresh` starts over from the base branch on a new branch and sets the old worktree aside, which worktree gc deletes, uncommitted work included, `worktrees.gc_after` (7 days) after the item's last run; `--append` adds guidance to the brief |
| `archon queue refresh <id>` | merge a finished item's base branch, fetched from `origin`, into its branch: a new item resolves any conflicts, runs the checks, is reviewed from the new merge base and updates the same pull request (never a rebase or a second pull request) |
| `archon queue steer <id> [--note "…"] [--stage <stage>] [--model <provider/model>] [--effort <effort>] [--budget-usd <usd>] [--extend <minutes>] [--abort [--then retry\|skip] [--goto <stage>]] [--remove <stage>] [--insert <stage> [--from <pipeline>] [--after <stage>]] [--switch <pipeline>]` | change an item without cancelling it; see [steering an item](#steering-an-item) |
| `archon log` | the complete feed; `--item <id>` keeps one item's entries, `--kind <kind>` (repeatable) some kinds, and `--follow` keeps printing new ones |
| `archon merge <id> [--ignore-checks]` | merge a succeeded item's pull request at its reviewed commit; see [Merging on request](#merging-on-request) |
| `archon reports` / `archon reports reopen <rpt_…>` | list published reports with their state and URL, or show a closed one again |
| `archon github-watcher …` | route `@archon` on GitHub issues and PRs to the orchestrator. `operators <LOGIN>...` lets the GitHub accounts you list instruct it with your authority (off by default); see [`docs/github-watcher.md`](github-watcher.md) |
| `archon pause` / `archon resume` | hold dispatch globally, per repo or per pipeline; `--hard` also pauses running agents |
| `archon gc [--offline]` | delete finished runs' build caches and remove their old worktrees now; `--offline` works without archond, e.g. on a full disk. See [disk use](configuration.md#disk-use) |
| `archon aether …` | Aether's CLI on Archon's own Aether |
| `archon daemon status` / `archon daemon stop [--keep-aether]` | both daemons' state; stop archond and, in dedicated mode, Archon's aetherd unless `--keep-aether` |

See [scripted queue usage](queue-scripting.md) for file formats, autonomy
flags, idempotency, JSON results and wait exit statuses.

Cost reports distinguish provider-reported billing, list-price estimates, and
unclassified legacy or mixed-basis amounts. The counted total follows budgets:
Aether's reported cost always counts, including list-price amounts; only its
separate API-equivalent estimate depends on `limits.count_estimated_cost`.
Since Aether 1.0.9 Codex usage is such an estimate (billed $0), so with the
default setting budgets count Codex as before; with it off, Codex counts $0
toward budgets and the report adds `≈ $X API-equivalent`. Learning and model
evidence always compare the API-equivalent total, billed plus estimated.
Finished run costs in `queue show` sum their own stages' current ledger costs,
so late usage stays with the correct run after a retry or restart. A stage's
cost includes the agents it delegated to; a run's and an item's totals count
each agent once. While a turn runs, Aether prices it at list price, and when
it ends the provider's own figure replaces that: a running total can come
down a little then, and budgets count the corrected figure.

Wherever a command takes a `qi_…` item id, the start of it is enough, with
or without `qi_` and in any case (`archon queue show 01k2`), as long as it
names one item. A start that several items share is refused with the list
of their ids and titles.

`--json` prints JSON instead of text. The commands that hand over the
terminal or stdout refuse it with an error: a bare `archon`, `archon chat`, `archon tui`,
`archon aether …`, `archon mcp` and a foreground `archon daemon run`. For
Aether's own JSON, put the flag after the Aether command, as in
`archon aether agents --json`.

After `--json` has been parsed, command failures print one JSON envelope on
stdout, for example `{"error":{"code":"not_found","message":"…"}}`, and
keep exit status 1. Text-mode errors still go to stderr. Codes use the daemon
and MCP vocabulary: `invalid_argument`, `not_found`, `invalid_transition`,
`policy`, `forbidden`, `unavailable`, `incompatible`, and `internal`.
Unclassified CLI failures use `internal`; argument-parser errors (before a
command is parsed) retain clap's stderr output and exit status 2. Commands
such as `doctor` that return a failed check report keep their existing JSON
report and exit status. Successful JSON output keeps its existing shape. For
`log --follow --json`, a failure envelope is the final JSON line after any
feed entries already emitted.

`archon status` observes the existing archond without starting Archon or
Aether, including in dedicated mode. A running archond reports
`aether: disconnected` during an Aether outage. If archond is also down,
the command returns an `unavailable` error (exit 1): its Aether link cannot
be observed. `--no-start` is still accepted, and changes nothing.

Queue detail and list views put `awaiting approval` first when the current
run needs a proposal/report decision or a blocking Aether question, with
the prompt and `archon answer …` command. Status shows these items before
its daemon summary. JSON queue detail/list responses add `waiting_on` with
`subject`, `stage`, `question`, `prompt`, and `next_action` while preserving
the stored `state` (`running`). It is omitted when nothing blocks the run;
answered questions, older runs, and advisory MCP questions do not imply a wait.

Queued and blocked items also carry `waiting_reason` (`code`, `message`,
`next_action`) in queue list/detail JSON and MCP `list_queue` / `get_item`.
Status includes those rows plus operator waits in `waiting_items`, in text
and JSON. An active item awaiting an operator answer has code `operator_gate`;
`waiting_on` remains available for its stage, question and approval subject.
Terminal items have no waiting reason.

Reasons are derived at read time from the dispatcher's snapshot and planner,
including pending spawns, repository config caps, team admission and capacity
reserved for earlier items. Precedence is: operator answer; failed/cancelled
dependency; unfinished dependency; global pause; low disk; total budget;
daily budget; repository pause; pipeline pause; capacity; `next_in_line`.
This follows the dispatcher's global hold order. Dependency reasons name one
dependency. A blocked item whose dependencies recovered still shows its block
until an explicit lifecycle action releases it. Each reason suggests one next
action; after that action, another hold may become visible. `next_in_line`
means the planner would admit it on the next pass; status separately reports
Aether disconnection, which stops dispatch.

`archon queue caps --budget-daily <usd>` and `--budget-total <usd>` raise
saved budgets until the global settings overlay is reset (0 removes the limit);
`--budgets off` stops enforcing every dollar budget until then (set
`budgets_enabled = false` under `[limits]` to make it permanent), and
`--budgets on` enforces them again. Spend is recorded either way. Daily budgets
reset at local midnight. Capacity suggestions inspect running work, or raise
a limiting cap when a team can never fit. Disk reasons use the dispatcher's
current hold, without performing a disk check or garbage collection on reads.

Failed-stage chat outcomes show a short cause and
`archon queue show <id> --full`; full stage evidence and orchestrator summaries
are retained. On known
OSC-8 terminals (including Kitty, Ghostty, WezTerm, iTerm2, VS Code, Windows
Terminal and modern VTE), chat reports show an `open report` hyperlink. Its
target contains the full URL or encoded absolute file path. Pipes, unknown
or dumb terminals, and `NO_COLOR` use the full location as plain text;
`archon reports` and JSON retain the full location in all cases.

### Which commands start archond

A command that only reads never starts `archond` or Archon's `aetherd`. With
no daemon running it prints ``archond is not running (start it with
`archon daemon start`)`` and exits 1 (`unavailable` under `--json`). These
are `status`, `questions`, `log`, `cost`, `model-evidence`, `queue list`,
`queue show`, `queue wait`, `queue caps` without a change, `repo list`,
`initiatives` and `initiatives show`, `autonomy` without a change,
`github-watcher` without a change, `orchestrator status` and `reports`.
`lessons`, `pipelines` and `doctor` read their files directly instead.

A command that changes something (`say`, `answer`, `queue add`, `cancel`,
`retry` and `steer`, `repo add`, `pause`, `lessons accept` and the other
lesson changes, `reports reopen`, `orchestrator restart`, …), the TUI, the
line chat and `archon daemon start` start archond when it is not running,
and in dedicated mode Archon's `aetherd` first. They say so on stderr, with
the data directory and socket the new archond owns. `archon mcp` starts
archond for Aether's agents in the same way, for a tool call that changes
something; a read-only tool call never starts it.

Archon keeps its files in three places: the config directory, the data
directory and archond's socket. `ARCHON_CONFIG_DIR`, `ARCHON_DATA_DIR` and
`ARCHON_SOCKET` move them, and a private or test setup must set all three.
Where only some of them are set, a command could start an archond that
serves the default store under a private name, so nothing starts archond:
commands that would start it fail and say which variables are set, reads add
the same note, and `archon doctor` warns. `archon daemon run` (foreground or
`--detach`) still starts archond, as it is asked for directly, and prints the
same warning.

One archond owns a data directory. It holds a lock on `<data>/data.lock`,
which records its pid and socket, from before it opens the store until it
exits. A second archond started on the same data directory, under any
socket, exits with an error that names the first one's pid and socket.
`archon gc --offline`, and tools that open the store to write, hold that
lock shared: they are refused while archond runs, and archond does not
start while they hold it. `archon lessons`, which reads the store
directly while archond is not running, opens it read-only and takes no
lock.

### Steering an item

`archon queue steer <id> …` changes an item while it runs, and the
orchestrator does the same with `steer_item`. Nothing is cancelled and
queued again: the agents at work keep their context and their worktree.
Give at least one change; several in one command are applied together.

| Flag | What it does |
| --- | --- |
| `--note "…"` | Adds the text to the brief under a dated `## Amendment` heading and sends it to every agent working on the item now, helpers included. Later stages read it in the brief. |
| `--model <provider/model>`, `--effort <effort>` | Changes the model or effort of `--stage` (the running stage when omitted), within the model policy. A running agent ends its turn and goes on with its work on the new model; later attempts and retries use it too. |
| `--budget-usd <usd>` | Sets the item's budget. Running agents that carry their own cap get the difference. |
| `--extend <minutes>` | Gives the stage running now more time, up to 1440 minutes a steer. |
| `--abort` | Stops the agents of `--stage` now. The stage is then started again with a new agent, or passed over with `--then skip`, or the run continues at another stage with `--goto <stage>`. |
| `--remove <stage>` | Takes out a stage that has not started, for this run. |
| `--insert <stage> [--from <pipeline>] [--after <stage>]` | Adds a copy of a stage, from the item's own pipeline or another, among the stages that have not started. |
| `--switch <pipeline>` | Replaces the stages that have not started with what that pipeline runs after the stages both share. |

```console
$ archon queue steer 01k2 --note "Use the v2 client, not the legacy one."
Steered qi_01K2…: amended the brief.
$ archon queue steer 01k2 --stage implement --model claude-code/claude-opus-5-5 --effort high
$ archon queue steer 01k2 --abort --goto implement
$ archon queue steer 01k2 --insert verify --from refactor --after implement
```

A stage that already started keeps its shape: an insert lands after it, a
switch keeps it, and a stage that ran and is not running now cannot be
removed. To run such a stage again, abort the running one with `--goto`.
The model policy, the budgets and the review your autonomy settings require
hold for a steer as they do for an enqueue, and a refusal says which rule
it met. Steers survive a daemon restart: the run goes on with the stages
and decisions they left.

An item that is not running takes what it runs next: a note, a budget, a
stage's model (name the stage), and an insert or a switch. `--abort`,
`--extend` and `--remove` need a running item. A failed item given
`--insert` or `--switch` is queued again on the same branch and worktree,
with what its failed run committed; this is how an item whose stage gave
up with `ESCALATE:` continues on a stronger pipeline instead of starting
over. A cancelled item takes no stage change and a succeeded one no steer
at all until it is retried. `archon queue retry` queues a succeeded item
again on its branch, delivering into the pull request it has; an item
waiting on it waits for it to succeed again, and the retry counts against
the run that passed.

Every steer is recorded with who made it (`operator` or `orchestrator`) and
what it changed. `archon queue show <id>` lists them, and the feed has a
`Steered …` entry for each. An item switched to another pipeline keeps the
name it was queued with, and `queue show`, `queue list` and `status` print
both, as in `direct → adaptive`.

### Stacked changes

An item that `depends_on` an unmerged item of its own repository stacks on it:
its branch forks from the first unmerged parent's, and `archond` opens its
pull request against that branch. Other parents are merged
in. A parent's merge retargets the child PR; later parent commits are merged
forward. Conflicts create a fix item on the child branch, visible in the queue.

### Coordinated changes across repositories

Ask for one initiative when a request spans repositories. Its shared contract
states the API names, formats and rollout rules each member must follow.
`enqueue_initiative` accepts a title, contract (up to 2 KiB), and 1 to 16 members with
`repo`, `brief`, optional `title`, `pipeline`, `model_plan` and `autonomy` (as
in an enqueue), and `after: [0, ...]` indices of earlier members. Supply an
`idempotency_key` when retrying a request. Set `atomic: true` to keep all PRs
draft until every member passes; they are then readied together and you merge
them, so an atomic member asking for `merge: auto` is refused. The daemon
adds a “Part of …” section with sibling PR links and reports one group outcome.

`archon initiatives` lists groups; `archon initiatives show ini_…` shows the
shared contract and members. `archon queue show qi_…` names its initiative.
`update_initiative` appends to the shared contract or replaces it when what was
agreed changes, and tells live member leads the whole new contract.
From the CLI, `archon initiatives create request.json` sends the same request
from a JSON or TOML file, `archon initiatives update ini_… --append-file
more.md` appends, and `--contract-file revised.md` replaces (`-` reads stdin
for either). Like an item, an initiative can be named by the start of its id:
`archon initiatives show 01k2`. `[limits] initiative_members` and
`initiative_contract_bytes` raise the 16 members and 2 KiB
([configuration](configuration.md#structural-limits)).

`archon initiatives cancel ini_…` stops unfinished members and leaves successful
branches and PRs in place. `archon initiatives retry ini_…` resumes failed,
cancelled and blocked members on their existing branches once no member is
active. Successful members are preserved. A member failure holds queued siblings;
retry the initiative to resume them. Retrying individual members does not reopen
a failed or cancelled group.

## Using Archon from Aether's remote app

This depends on an Aether build that has the remote app (a web app
`aetherd` serves to a paired phone or browser). It has not been tested
against one: what follows is what Archon's side provides.

The remote app shows an Aether instance's agents and their chats. Archon's
orchestrator is an agent of **Archon's own** Aether instance, so the app has
to be served by that instance, not by your everyday Aether:

- Enable and pair the remote listener on Archon's instance, with
  `archon aether remote …` in place of `aether remote …`.
- Give that instance its own `[remote] address` in
  `~/.config/archon/aether/config.toml`. The default, `127.0.0.1:8377`, is
  the one your own Aether uses, and two daemons cannot bind it. Restart
  Archon's aetherd afterwards (`archon daemon stop`, then `archon`).
  `archon setup --resync` rebuilds that file from your own Aether config, so
  set the address again after a resync.

What works from the app, because it is all text in the orchestrator's chat:

- Talking to the orchestrator, and reading its replies, item outcomes and
  the stage messages it echoes.
- Seeing a gate: when a proposal or report gate opens, the orchestrator
  restates it with its number and a summary.
- Answering a gate or question: send `/answer <n> approve`,
  `/answer <n> reject` or `/answer <n> <feedback>` as the whole message.

What does not: the Lavish page of a gate is a link to `127.0.0.1` on
Archon's machine and does not open on a phone; the `archon` CLI, its line
chat and `archon questions` are not in the app; there are no Archon views
and no push notifications for gates. Chat answers need the default
dedicated mode (see
[Answering in the orchestrator's chat](#answering-in-the-orchestrators-chat)).
A device paired as an observer cannot answer: Aether does not treat it as
you.

## Autonomy levels

Each item carries five knobs. The orchestrator sets them from the change's
risk and from what you told it, within a ceiling that only you can raise.

| Knob | Values (default first) | What it changes |
| --- | --- | --- |
| `review` | `standard`, `strict`, `none` | how much independent checking the change gets. With no pipeline named the item runs `direct`; in `adaptive` or `direct` the stage agent delegates one, three or no reviewers and Archon verifies them. Any other pipeline has the level its gated stages give it, whatever its reviewer count: a gated review or test stage makes it `standard` and both make it `strict`, so `refactor` is `strict` with one reviewer |
| `proposal` | `off`, `lavish` | `lavish`: a proposal page first, and nothing is built until you approve it |
| `report` | `off`, `with-pr`, `before-pr` | a change report in the PR; `before-pr` waits for your approval before the PR |
| `merge` | `manual`, `auto` | `auto`: Archon asks GitHub to merge the PR once its checks allow |
| `tools` | `guarded`, `yolo` | `guarded`: a stage agent delegates only when its stage sets `allow_delegation`. `yolo`: every stage agent may delegate, and nothing waits for approval |

Answer a waiting proposal or report in chat with `/a <n> yes`,
`/a <n> <feedback>` or `/a <n> reject`, with `archon answer <n> …`, on
its Lavish page, or with `/answer <n> …` in the terminal UI (see
[Answering in the orchestrator's chat](#answering-in-the-orchestrators-chat)).
A question keeps its number for good: if it was answered
elsewhere in the meantime, the answer is refused instead of reaching another
question.

The built-in ceiling allows everything except `merge = auto`. You set the
defaults and the ceiling in `[autonomy]` in the config, or per repository
with `archon repo set <repo> --autonomy-max review=none,merge=auto`. A
repository's own `.archon/config.toml` can only lower them.
`archon autonomy direct "…"` records a standing instruction, such as
"auto-merge docs-only changes today". `archon autonomy` shows the defaults,
the ceiling and the directives. See
[`docs/ARCHITECTURE.md` §18](ARCHITECTURE.md#18-autonomy).

### Merging on request

`merge = manual` leaves a pull request for you to merge, but you can have
Archon merge one without raising the ceiling:

```sh
archon merge <item>                  # also /merge <item> in archon chat
archon merge <item> --ignore-checks  # merge although checks are red or pending
```

Archon merges only an item that succeeded and whose pull request is open,
not a draft, mergeable, on the right base, with no unmerged stack parent,
and still at the commit its review passed. It merges by the first method the
repository allows, pinned to that commit, never with `--admin`. Checks
must be green unless you acknowledge them. A refusal says why, is recorded,
and merges nothing. `archon queue show <item>` prints a `merge:` line.

You can also just tell the orchestrator "merge it", in chat or in a GitHub
request with operator standing. It cannot merge by itself: it calls
`request_merge`, and Archon asks you `Merge …?` as a numbered question.
Answer `/a <n> yes`, `/a <n> yes ignore-checks` or `/a <n> no`. The
orchestrator cannot answer it for you. A whole-message `/merge <item>` in
the orchestrator's Aether chat merges at once, in dedicated mode only.

To skip that question for a repository, grant it:

```sh
archon repo set <repo> --grant-merge true   # false withdraws it
```

The grant has no expiry and is independent of `merge = auto`. Under it, a
request for a reviewed item whose checks are all green merges without a
question. Red checks, or an item with `review = none`, are still asked.
`archon autonomy` and `archon status` show the grant, and `archon status`
any merge waiting for you. See
[`docs/ARCHITECTURE.md` §18.6](ARCHITECTURE.md#186-merges-the-operator-asks-for).

### Who speaks as you

The orchestrator takes instructions from you in the terminal UI,
`archon chat` or `archon say`, and from a
relay Archon authenticated as yours. A request from GitHub is an external
request: the orchestrator answers, researches and queues work within what
you already allowed, and will not record directives, register repositories,
pause globally, or choose `merge: auto` or `tools: yolo` for it, whoever
wrote it.

`archon github-watcher operators <LOGIN>...` changes that for the GitHub
accounts you list, and only for them: their own requests then carry your
authority, up to the ceiling. It is off until you set it, and
`archon github-watcher operators --clear` turns it off again. A listed
account is as powerful as you are at your terminal, and losing it means
losing that authority, so list only accounts you would hand your terminal
to. Aether decides whether a request qualifies (the login, its right to
dispatch on that repository, and that it alone wrote the text); Archon only
passes that decision on. Only you, at your terminal, can set the list: no
GitHub request, agent or MCP tool can. After an upgrade, run
`archon orchestrator restart` before you set it: archond refuses a list
while the orchestrator that takes GitHub requests runs older instructions,
which do not know operator standing. See
[Operator logins](github-watcher.md#operator-logins).

Authority is checked by archond, not by the model. Whole-message commands
(`/answer`, `/approve`, `/reject`, `/merge`, `/archon set --repo …`,
`/archon pause --hard`, `/archon repo remove`, `/archon undo N`) work from
your chat and from a listed GitHub operator. In prose, the orchestrator
cites your message with `request_authority`; a change that loosens a limit
comes back as a question only you can answer, and applies only if nothing
changed meanwhile. Every use is in the feed as an Authority entry, and
repository and caps changes can be undone. See
[Operator authority](operator-authority.md).


## Schedules

Scheduling belongs to Aether. Archon has no separate schedule command.
Use `archon aether schedule --help` for the scheduling interface of the
installed Aether version. It supports recurring inbox delivery and fresh-run
schedules. Target Archon's orchestrator for recurring requests that should
enter its queue; a fresh Aether run does not itself become an Archon item.
See [Aether's documentation](https://github.com/mengsig/aether).

For benchmark results and the crate layout, see [development and benchmarks](development.md).

## Lean host context

To reduce the host tools loaded into newly created Claude Code sessions, set
`host_context = "lean"` under `[aether]` in `config.toml`. Full context remains
the default pending quality benchmarks. Lean removes skills and non-Aether
host MCP servers while keeping Aether's MCP (including Archon's tools),
built-in tools, CLAUDE.md, hooks and settings. Codex ignores the setting.

Restart `archond` to reload the config, then use `archon orchestrator restart`
for a new orchestrator; already created agents and resumed fix attempts keep
their original context. Run `archon doctor` to see the requested and effective
policy. If Aether does not advertise support, Archon creates agents with full
context and logs a warning. Pending lean requests also fall back to full
if support disappears, with one warning per changed stage or node. Stage
records and `archon model-evidence` retain the effective policy for comparing
cost and outcomes; Codex stages always record full. See the
[configuration reference](configuration.md) for the full contract.
