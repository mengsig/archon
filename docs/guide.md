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
`model_reasons` explanation and can query `model_evidence`. The daemon
checks choices against your allowlist, caps and budgets.

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
2. The orchestrator plans the work and calls `enqueue` once per unit of work,
   choosing a repository, a pipeline and a model and effort for each stage.
3. `archond` dispatches items as capacity allows. It creates a git worktree
   for each item and runs the pipeline's stages as Aether agents.
4. The pipeline enforces review and test gates and fix loops. Outcomes and
   blockers come back to the orchestrator, and it tells you only what
   matters.

## Agents talking to the orchestrator

Every stage agent can call Archon's `message_orchestrator(text, kind)` in
guarded and yolo modes. Use `relay` to pass on an operator message sent to
the wrong agent, `escalation` for a blocker outside the stage, or `info`
(the default). Text must be non-blank and at most 4096 characters. The
tool returns a receipt immediately; the agent must continue its stage
without waiting for a reply or ending its turn.

Archon records item, run, stage, attempt and agent attribution in the
item log (`archon log --item <id>`) and delivers an `agent_message`
news event to the orchestrator. Delivery is durable and coalesced; each
agent can send at most 20 messages, with a clear refusal after that.
Delivered messages to passive run nodes are forwarded through the same
path, including late events for retired nodes whose normal index was pruned. External
messages have a separate 20-message cap per run node; refusals reach the
sending agent or the operator feed. Use the tool for stage messages.

Relay wording alone grants no operator authority. A relay has operator
provenance only for an authenticated operator inbox sender, or when a
stage quotes exactly the latest operator message Archon observed in its
inbox. Other relays retain agent provenance. The orchestrator can answer
using Aether `send_message`, a retry with `brief_append`, or new items.

New `archon setup` registrations enable Aether's `send_caller_meta = true`.
In dedicated mode, `archon setup --resync` also migrates that registration.
For an existing shared registration, add that field to the `archon` MCP server's
`[[mcp.servers]]` entry in the active Aether config and restart that Aether daemon;
also add `message_orchestrator = "mutation"` to its `classes` table.
Calls without caller identity are refused. Setup preserves existing entries.

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
| `quick-pr` | implement → ship | fast models |
| `default-pr-review` | implement → review (gate, fix loop) → ship | balanced models |
| `bugfix` | reproduce (failing test) → fix → review (`all_pass`, back to fix ×2) → ship | balanced models, independent reviewer; describable bugs (`quick-pr` for trivial one-liners, `strict-pr-review` for security/concurrency) |
| `docs` | write → verify (all claims, commands and internal links; gate, back to write ×2) → ship | documentation with an independent reviewer; fast/balanced models. Use `quick-pr` for a typo. |
| `dependency-upgrade` | survey → upgrade → verify (build/tests/audit, back to upgrade ×3) → ship | balanced models; version bumps, with changelog research; use `default-pr-review` for features that add dependencies |
| `refactor` | baseline/characterize → refactor → verify (gate, back to refactor ×3) → review (gate, back to refactor ×2) → ship | balanced tier, independent reviewer; structure-only changes, use `default-pr-review` for behaviour changes |
| `strict-pr-review` | implement → test (gate) → 3 independent reviews (all must pass) → ship | the strongest models |
| `team-pr` | plan → build (a `backend` and a `frontend` role in parallel) → e2e (gate, fix loop) → review (gate, fix loop) → ship | changes that split cleanly into parts |
| `performance` | measure → optimize → verify (gate, back to optimize ×3) → review (gate, back to optimize ×2) → ship | strongest tier, independent reviewer. For a specific slow path with a measurable goal; use `default-pr-review` for ordinary changes. |
| `test-coverage` | analyze → write → verify (`all_pass`, back to write ×2) → ship | balanced models; raise coverage of existing code, with bugs reported as follow-ups; use `bugfix` to fix one |
| `agent-testing` | plan → exercise (fan-out) → report | agents using the product as a user would |
| `code-audit` | scope → audit (3 independent lenses) → report (HTML with issue drafts) | code inspection with evidence and follow-up findings; use `ask` for one question |
| `e2e-testing` | setup → e2e (gate) → report | HTML and screenshot reports for Lavish |
| `ask` | answer | a read-only answer about a repository, with no PR |
| `errand` | errand (report) | a small job that changes no repository, answered in text or as a Lavish page; no outward side effects |
| `design-doc` | research → critique (gate, retry ×2) → report (HTML design/RFC) | design before a large or ambiguous change, strongest tier and independent critic; no product-code changes or shipping. Use `proposal: lavish` for design and implementation in one item |

A work item ships in one of two modes, set per repository:

- `pr` (the default): opens a PR. Merging requires the item's `merge = auto`
  knob and permission in your ceiling (`--auto-merge` permits it for a repository).
- `local`: commits on the run branch and never pushes.

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
  `Trigger from archon · item finished: qi_… succeeded — <title>`.
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
  actor. An agent's message, a relay, a GitHub request, and a `/answer`
  quoted inside other text answer nothing. The orchestrator has no tool that
  answers a gate.
- A plain reply to a question the orchestrator asked itself (`ask_operator`)
  works as before, and the orchestrator now records it, so the question
  leaves `archon questions`, marked
  `answered in chat, recorded by the orchestrator`. It can do that only for
  its own questions, never for a gate. This depends on the orchestrator
  following its instructions; `archon answer <n> …` always closes it.
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

`archon` starts Archon's `aetherd` and `archond` when they are not running.
`archon aether <args…>` runs any `aether` command against Archon's instance,
for example `archon aether agents`. `archon daemon stop --aether` stops both
daemons. In the TUI's tree each work item is one subtree: a node named after
the item, with its stage agents (`implement · <title>`, `review 1/2 · <title>`,
`ship · <title>`) underneath, beside the `archon` orchestrator.

```sh
archon repo add ~/code/api --ship pr
archon repo add ~/code/web --pipeline strict-pr-review
```

In `archon chat`, just talk. Slash commands cover the rest: `/questions`,
`/a [n] <answer>` (the newest open question without `n`),
`/answer <n> <answer>`, `/status`, `/queue [state …]` (the queue, e.g.
`/queue running failed`), `/item <id>` (one item), `/progress` (show stage
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
| `archon queue add --title … --brief-file <path\|-> --model-plan-file <json\|toml>` | enqueue scripted work through the daemon's MCP-equivalent policy checks; prints the id or the item with `--json` |
| `archon queue wait <id>… [--timeout 30m]` | wait for every item to become terminal, bounded to 30m by default; exit 0 all succeeded, 3 failed/cancelled, 4 timeout, 5 failed/cancelled dependency |
| `archon queue list` | one line per item: state, current stage, age, cost, title, PR URL and derived waiting reason with one next action |
| `archon queue show <id> [--full]` | an item's runs and each stage's attempt, slot, state, verdict, model, agent, cost and summary (first lines; `--full` for all), with the PR URL, recorded plan reasons and full waiting explanation when present |
| `archon queue cancel <id>` / `archon queue caps` | cancel an item, or show and change runtime caps |
| `archon queue retry <id> [--fresh] [--append "…"]` | queue a failed or cancelled item again in its worktree and branch; `--fresh` starts over from the base branch on a new branch and sets the old worktree aside, which worktree gc deletes, uncommitted work included, `worktrees.gc_after` (7 days) after the item's last run; `--append` adds guidance to the brief |
| `archon log` | the complete feed; `--item <id>` keeps one item's entries, `--kind <kind>` (repeatable) some kinds, and `--follow` keeps printing new ones |
| `archon reports` / `archon reports reopen <rpt_…>` | list published reports with their state and URL, or show a closed one again |
| `archon github-watcher …` | route `@archon` on GitHub issues and PRs to the orchestrator. `operators <LOGIN>...` lets the GitHub accounts you list instruct it with your authority (off by default); see [`docs/github-watcher.md`](github-watcher.md) |
| `archon pause` / `archon resume` | hold dispatch globally, per repo or per pipeline; `--hard` also pauses running agents |
| `archon gc [--offline]` | delete finished runs' build caches and remove their old worktrees now; `--offline` works without archond, e.g. on a full disk. See [disk use](configuration.md#disk-use) |
| `archon aether …` | Aether's CLI on Archon's own Aether |
| `archon daemon status` / `archon daemon stop [--aether]` | both daemons' state; stop archond, and with `--aether` Archon's aetherd too |

See [scripted queue usage](queue-scripting.md) for file formats, autonomy
flags, idempotency, JSON results and wait exit statuses.

Cost reports distinguish provider-reported billing, list-price estimates, and
unclassified legacy or mixed-basis amounts. The counted total follows budgets:
Aether's reported cost always counts, including list-price amounts; only its
separate API-equivalent estimate depends on `limits.count_estimated_cost`.
Finished run costs in `queue show` sum their own stages' current ledger costs,
so late usage stays with the correct run after a retry or restart.

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

`archon status --no-start` observes the existing archond without starting
Archon or Aether, including in dedicated mode. A running archond reports
`aether: disconnected` during an Aether outage. If archond is also down,
the command returns an `unavailable` error (exit 1): its Aether link cannot
be observed. Ordinary `archon status` retains autostart behavior.

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
runtime budgets until archond restarts (0 removes the limit). Daily budgets
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
  Archon's aetherd afterwards (`archon daemon stop --aether`, then `archon`).
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
| `review` | `standard`, `strict`, `none` | `default-pr-review`, `strict-pr-review` or `quick-pr` |
| `proposal` | `off`, `lavish` | `lavish`: a proposal page first, and nothing is built until you approve it |
| `report` | `off`, `with-pr`, `before-pr` | a change report in the PR; `before-pr` waits for your approval before the PR |
| `merge` | `manual`, `auto` | `auto`: the ship stage merges its own PR |
| `tools` | `guarded`, `yolo` | `yolo`: the item's agents may delegate, and nothing waits for approval |

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


## Schedules

Scheduling belongs to Aether. Archon has no separate schedule command.
Use `archon aether schedule --help` for the scheduling interface of the
installed Aether version. It supports recurring inbox delivery and fresh-run
schedules. Target Archon's orchestrator for recurring requests that should
enter its queue; a fresh Aether run does not itself become an Archon item.
See [Aether's documentation](https://github.com/mengsig/aether).

For benchmark results and the crate layout, see [development and benchmarks](development.md).
