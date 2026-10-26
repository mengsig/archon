# Operator authority

Some changes are yours alone: raising a ceiling, granting merges, pausing
hard, removing a repository. This page says which controls those are, how
you exercise them from each channel, and what archond checks before it acts.
Plain-language instructions work: you tell the orchestrator, and archond,
not the model, decides whether you really said it.

- [The model](#the-model)
- [Channels](#channels)
- [Inventory of operator-only controls](#inventory-of-operator-only-controls)
- [Commands](#commands)
- [Delegated authority: `request_authority`](#delegated-authority-request_authority)
- [Confirmations](#confirmations)
- [The Authority feed and undo](#the-authority-feed-and-undo)
- [Configuration](#configuration)
- [Versions and provenance](#versions-and-provenance)
- [Storage decision](#storage-decision)
- [Limits and follow-ups](#limits-and-follow-ups)

## The model

Operator provenance is a fact archond observes on Aether's event stream. It
is never an argument a tool call supplies and never text a message contains.

1. archond records every inbox message that carries operator provenance as
   an *operator message* (agent, inbox sequence, channel, text, original
   event time). A replay of the same sequence cannot change the record or
   refresh its time.
2. A whole-message command from that sender is applied by archond itself.
3. For anything said in prose, the orchestrator calls `request_authority`
   and cites the message. archond looks the citation up in its own record.
4. A change that loosens something is not applied on the citation: archond
   asks you, with an exact question, and applies it only on your answer
   ([Confirmations](#confirmations)).

The orchestrator can therefore ask for authority and can be refused, but it
cannot grant itself any. Wording such as "[authenticated operator
instruction]" in a message, a relay, or a field in a GitHub payload grants
nothing.

## Channels

| Channel | Who is the operator | Notes |
| --- | --- | --- |
| `archon` CLI (operator socket) | the Unix user running `archon` | Reaches operator-only requests; `/archon undo` is available inside `archon chat`. It is your own socket, so there is no citation to check. |
| Aether terminal UI / remote app | the Aether `user` actor in **dedicated** mode | Commands and prose. In `[aether] mode = "shared"` a `[listen]` client is the same actor as you, so chat authority is refused there. |
| `archon chat` commands | the operator socket | `/archon`, `/approve` and `/reject` work even in shared mode. Plain text is forwarded to Aether and follows its dedicated-mode rules. |
| GitHub | a login you list with `archon github-watcher operators` | Only requests Aether's watcher envelope marks as an operator's own (top-level `operator` flag, Aether protocol 17 or newer). Text markers and payload fields alone are ignored. The command goes first: `@archon /approve 1`. |
| Orchestrator MCP (`request_authority`) | nobody: it cites a verified message from one of the channels above | Only the current orchestrator may call it. Delegated agents, stage agents and relays are refused. |

Agents' own messages, relays, shared-mode chat and unverified senders never
count.

## Inventory of operator-only controls

"Operator-only" means `archon mcp` and agents are refused the request on
archond's socket (`Request::is_operator_only`). The columns say how each is
reached today. A GitHub operator is scoped to the repository of the thread the
command is posted in (the registered repository whose `origin` it is, and only
while that repository's allowlist includes the login): it has full operator
rights there, and none over any other repository or any global setting. Every
loosening from GitHub, explicit or not, asks for confirmation first.

| Control | CLI | Chat / TUI | GitHub | `request_authority` | Confirmation |
| --- | --- | --- | --- | --- | --- |
| Answer a question or gate (`answer`) | `archon answer` | `/answer`, `/approve`, `/reject` | items of this thread's repository only | proposal/report gates only | none for gates; authority and merge confirmations are answered only by your own reply |
| Merge an item (`merge_item`) | `archon merge` | `/merge ITEM [--ignore-checks]` | items of this thread's repository only | no (the orchestrator has `request_merge`, a question you answer) | — |
| Repository ceiling `autonomy_max` | `archon repo set --autonomy-max` | `/archon set --repo R max.K=V` | this thread's repository only | `repo_update` or `set_settings` | when it raises any knob |
| `grant_merge` | `archon repo set --grant-merge` | `/archon set --repo R grant_merge=true` | this thread's repository only | `repo_update` | when set to true |
| `merge_waive_checks` | — | `/archon set --repo R merge_waive_checks=next` (empty clears) | this thread's repository only | `repo_update` | when it adds a branch |
| `attribution` | `archon repo set --attribution` | `/archon set --repo R attribution=none` | this thread's repository only | `repo_update` | any change |
| `gh_user` | `archon repo set --gh-user` | `/archon set --repo R gh_user=LOGIN` (empty clears) | this thread's repository only | `repo_update` | any change |
| `ship`, `base_branch`, `default_pipeline` | `archon repo set` | `/archon set --repo R ship=pr …` | this thread's repository only | `repo_update` | `ship` local→pr, and any `base_branch` or `default_pipeline` change |
| Verification tiers (`integration_branch`, `release_branch`, fast and full checks, the confirmed committed fast check) | `archon repo set --integration-branch …` | — | — | `repo_update` | any change |
| Saved defaults, ceiling, budgets and pipeline | `archon settings [--repo R] --set JSON` / `--reset` | `/archon set [--repo R] key=value` / `reset` | this thread's repository only (`--repo R`); global **refused** | `set_settings`, `reset_settings` | when effective or stored permission rises, a budget loosens, or the default pipeline changes |
| Item autonomy after enqueue / delivery | socket `update_item` | `/archon item qi_… merge=auto` | items of this thread's repository only | autonomy-only `update_item` | when a changed knob exceeds its ceiling |
| Caps and budgets (`set_caps`) | `archon queue caps` | `/archon apply {…}` | **refused** (global) | `set_caps` | when any limit rises, is cleared (zero) or budgets are turned off |
| Hard pause | `archon pause --hard` | `/archon pause --hard` | **refused** (global) | `pause` | no |
| Hard resume | `archon resume --hard` | `/archon resume --hard` | **refused** (global) | `resume` | always |
| Register a repository with `auto_merge` (`repo_add`) | `archon repo add` | `/archon apply` | **refused** (global) | `repo_add` | always |
| Remove a repository | `archon repo remove` | `/archon repo remove R` | this thread's repository only | `repo_remove` | always |
| Directives (`set_directive`, `clear_directive`) | `archon autonomy direct` / `clear` | `/archon apply` | **refused** (global) | yes | `clear_directive` only |
| Lessons and learning corrections | `archon learning …` | `/archon apply` | **refused** (global) | `update_lesson`, `learning_*` | always |
| GitHub watcher configuration | `archon github-watcher` | `/archon watcher JSON` | **refused** | `configure_github_watcher`, operator chat only | always |
| Undo an authority change | — | `/archon undo N` | same; the undone change must be in scope | `undo_authority` | same rules as the inverse change; guarded by unchanged state |
| Say something as the operator (`say`) | `archon say` | — | — | no | — |
| Restart / attach the orchestrator | `archon orchestrator restart`, `attach` | not implemented | not implemented | no | — |
| Drained daemon upgrade / restart | `archon daemon upgrade` | `/upgrade` (existing dedicated-TUI command) | no | no | explicit command only |
| One-command update (`archon update --yes`: install, `aether update`, drained restart, orchestrator restart) | `archon update`, or the offer at launch | `/upgrade both latest` restarts without the orchestrator step | no | no | explicit command or answer only |
| Shutdown, setup, destructive GC | CLI only | no | no | no | — |

The confirmation column applies to plain-language requests relayed through
`request_authority`. Explicit whole-message commands apply directly, since
the operator has named the exact action. Tightening (lower ceiling, hard pause, lower caps) applies
directly. An action whose direction cannot be ordered safely is treated as
loosening.

## Commands

A message that is exactly one of these is applied by archond. On GitHub put
`@archon` first. Anything else in the message makes it ordinary text.

These are chat commands: send them as a message in the Archon chat (or on
GitHub), not in a shell, so `archon --help` does not list them. Their
terminal counterparts are `archon answer`, `archon merge`, `archon settings`
and the other subcommands of `archon --help`.

```text
/answer N <answer>
/approve [N]                  /reject [N] [reason]
/merge ITEM [--ignore-checks]
/archon answer|approve|reject|merge …   (the same as the four lines above)
/archon set --repo NAME default.merge=auto max.merge=auto default_pipeline=adaptive
/archon set --repo NAME grant_merge=true ship=pr base_branch=main
/archon set budgets_enabled=false
/archon reset [--repo NAME]
/archon item qi_… merge=auto
/archon pause --hard          /archon resume --hard
/archon repo remove NAME
/archon watcher <change JSON>        # operator chat only
/archon apply {"cmd": …, …}          # any allowlisted action, as JSON
/archon undo N                       # N from the Authority feed
```

Details:

- `/archon set` edits the global overlay, or a repository's with `--repo`.
  `default.<knob>` and `max.<knob>` change defaults and ceilings. Other keys
  match `OperatorSettings`, including `budgets_enabled`, `budget_usd_per_item`,
  `default_pipeline` and `checks`. Omitted fields are preserved.
- `checks=waived` (`{"checks":"waived"}` in JSON) tells Archon the operator
  does without pull request checks, for example while GitHub Actions billing
  fails: checks that never start are a quiet feed line, never a wake of the
  orchestrator or an operator blocker. Unset (or `watched`), such checks are
  a quiet feed line first and wake the orchestrator once if they are still
  not started 30 minutes later. A pull request with no checks at all is a
  quiet feed line either way. Checks that ran and failed are told either way.
- Registry settings `grant_merge`, `merge_waive_checks`, `ship`, `base_branch`, `attribution` and `gh_user` use a separate
  `/archon set --repo` message; mixing them with overlay fields is refused.
- `/archon reset [--repo NAME]` clears the scope's overlay. Registry settings
  are separate and remain until explicitly changed.
- `/archon apply` accepts only a finite allowlist: repository add, update and
  remove; settings set/reset; autonomy-only item patches; caps; pause and resume; directives; lessons and learning
  corrections; watcher configuration. Shutdown, setup, update, destructive
  GC, nested authority requests and everything else are refused.
- A command is limited to 4000 characters.
- Settings and `archon queue caps` changes now survive daemon restarts. Reset
  the global settings overlay to return caps to the configuration file.
- Explicit commands from your own channels apply directly, including
  loosening. From GitHub a command is scoped to its thread's repository, and
  a loosening one asks for confirmation (or is refused with
  `github_may_loosen = false`).
- `archon chat` preserves a leading `//` escape when forwarding literal text
  to Aether, so the delivered message cannot become a command.

## Delegated authority: `request_authority`

When you say it in prose ("let `web` merge on its own"), the orchestrator
calls `request_authority` with:

- `utterance`: the inbox sequence of your message;
- `quote`: text that occurs verbatim in that message, on word boundaries, of
  at least 3 words or 12 non-space characters unless it is the whole message;
  for an `answer` it must also contain the answer's first word, with no
  `no`, `not`, `never`, `nor`, `cannot`, `dont` or word ending in `n't` up
  to 4 words before it in the same clause ("don't approve it yet" backs no
  `approve`; "no, approve it" does);
- `reason`: one line, 1–500 characters;
- `action`: one command JSON from the allowlist above, or an `answer` for a
  proposal or report gate.

archond refuses unless every check passes, in this order:

1. The caller is the current orchestrator, by Aether's caller metadata.
2. The cited sequence is an operator message archond recorded: from the
   dedicated Aether `user` actor, or a GitHub operator envelope. Agent
   messages and relays have no record.
3. In shared mode, chat citations are refused.
4. The message is at most 60 minutes old by its original time.
5. The quote occurs verbatim in it and is meaningful (see above).
6. The action is on the allowlist, and for GitHub is not a watcher change and
   stays within the repository of the cited thread.
7. For `answer`, the message arrived after the gate opened, and the question
   is a proposal or report gate. Authority and merge confirmations are not
   answerable this way.
8. If the change loosens, archond asks a confirmation and stops
   ([Confirmations](#confirmations)); with `authority.github_may_loosen =
   false`, a GitHub citation that would loosen is refused instead.
9. Otherwise it applies the change and writes the Authority feed.

The orchestrator is told to restate the confirmation question to you and
never to claim success before archond reports it. The
[`authority`](../crates/archon-daemon/prompts/topics/authority.md) guide topic
holds the action formats and is loaded on demand.

## Confirmations

A loosening request becomes a numbered question with the exact action and
the state it was asked against. You answer it yourself:

- `/answer N yes` or `/answer N no` in the same channel, or `archon answer`.
- A bare `yes`/`no` works only when exactly one confirmation is
  waiting (an open merge confirmation also makes a bare reply ambiguous), it is the **first** operator message you send in that channel after
  the question, and on GitHub it is in the same thread and from the same
  login. Any other first message uses up the bare reply; use the numbered form
  then.
- Yes is accepted only if the affected settings are unchanged since the
  question was asked. Otherwise archond refuses and you request again.
- The record moves waiting → executing → applied (or declined) before and
  after effects, so a crash cannot replay an approved change.

The orchestrator has no tool that answers a confirmation, and
`request_authority` refuses to.

## The Authority feed and undo

Every use of authority is a typed `Authority` feed entry, shown in
`archon log`, the terminal UI and Aether's remote app: the source channel
(and GitHub thread), the cited message, the requested action, the state
before and after, and the outcome. The outcomes you will see are
**requested**, **awaiting operator**, **applied**, **declined** and
**refused: reason**.

An overlay, repository setting, item-autonomy or caps change that was applied ends with
`undo with /archon undo N`. Undo restores the previous repository settings
(ceiling, `grant_merge`, `attribution`, `gh_user`, `ship`, base branch, default pipeline) or the previous saved overlay. Item autonomy restores the previous knobs
subject to current item-state rules. It
is refused if those settings have changed since: it never overwrites a newer
change. A cited undo asks when it restores a looser value; an explicit
`/archon undo` applies directly. Undo is not offered for pause, directives,
removals or watcher changes.

The orchestrator is instructed to omit private settings, spend, paths and
other repositories from GitHub replies. Confirmation details returned to it
include local state: public reply redaction currently relies on that prompt,
not a daemon-enforced output filter.

## Configuration

```toml
[authority]
github_may_loosen = true   # default: verified GitHub operators may approve loosening
```

`true` lets a verified GitHub
operator authorize the same allowlisted changes as a chat operator. Set it to `false` to let
GitHub only tighten: a loosening command or citation from GitHub is refused,
not asked. Nothing else changes with it. See
[configuration](configuration.md).

## Versions and provenance

- **Archon protocol 13** adds daemon-verified authority requests
  (`request_authority`, `undo_authority`) and the `guide` request. Older
  archond versions do not know them.
- **Aether protocol 17**, the oldest Archon speaks, carries the watcher
  envelope's `operator` flag that GitHub operator standing is read from.

## Storage decision

Operator messages, confirmations and undo records live in archond's existing
key-value store, under `operator_message:`, `authority_confirmation:` and
`authority_change:`. A dedicated schema migration was considered and
rejected: the records are small, short-lived (messages are pruned after seven
days), keyed by the feed sequence they relate to, and read by key only, so a
migration would add upgrade risk for no query that needs it. The Authority
feed is the durable, user-visible record; the key-value rows serve it.

## Limits and follow-ups

- **Not implemented:** restarting the orchestrator from the terminal UI,
  and a full-auto mode (`archon` acting on the operator's standing intent
  across repositories). Both remain follow-ups.
- Shutdown, setup and destructive GC remain CLI only. Existing dedicated-TUI
  `/upgrade` commands remain available; GitHub cannot invoke them.
- `request_authority` depends on the orchestrator following its
  instructions to cite and to restate. archond enforces the checks above
  regardless, but cannot make the orchestrator ask.
- A GitHub operator is as powerful as your terminal for the allowlisted
  actions above. List only accounts you would hand it to
  ([Operator logins](github-watcher.md#operator-logins)).

Saved settings are available through `archon settings [--repo R]`, its `--set`
JSON and `--reset` options, and the read-only MCP `settings` tool. Precedence is
configuration file < global overlay < registry and repository overlay < the
repository's `.archon/config.toml` autonomy limits, which can only lower.
Repository defaults and ceilings may raise or lower the global values. Values
are resolved for new items; existing items use their saved knobs. Per-field
sources are shown by `settings`; status and autonomy show effective values.

Per-repository limits support the budget switch, per-item cap and
`max_per_repo` agent limit (which may raise or lower the global per-repo default). Daily/total budgets
and other concurrency settings remain global; attempts to set them per repository are
refused. These broader per-repository controls are a follow-up. Global daily and
total amounts still use global spend; a repository's switch controls whether
those holds apply to it. Already running Aether agents retain their granted
budgets; switching off applies to future spawns and pending unbound grants.

`update_item` and `steer_item` accept `autonomy` within the effective ceiling.
An above-ceiling override uses `request_authority` with an autonomy-only
`update_item`, or an explicit `/archon item` command. Queued items that never
ran can change all knobs; a running item can change only merge/tools, and tools
affect future spawns. A succeeded item accepts only merge changes. `merge=auto`
requires an open PR at the delivered, reviewed commit and expected base; the
GitHub call uses `--match-head-commit`. `merge=manual` withdraws an auto-merge
still owed locally; an attempt already in flight can still reach GitHub. It
does **not** disable auto-merge already enabled there. Atomic-initiative
members remain manual even after success and even with a ceiling override.
Stage-changing autonomy cannot be combined with `model_plan` in one update.

An item ceiling exception is saved atomically with its knobs. It covers only
the exact approved values of explicitly named knobs on that item, survives
daemon restart and retry, and remains valid if the repository ceiling later
changes. Changing an approved knob revokes its exception; returning to that
value needs fresh authority if it exceeds the ceiling. Other knobs still
face the current ceiling at dispatch. These exceptions never transfer to a
new item or change the repository's ceiling.

Prompt budgets for this feature: core orchestrator prompt ≤ 12 KiB, each
guide topic ≤ 4 KiB, the MCP tool list ≤ 28 KiB, a new tool ≤ 1.5 KiB.

The dedicated Aether socket trusts the Unix account. Its local connections,
including archond, send messages with the `user` actor. Only genuine operator
`Say` messages may be forwarded to the orchestrator with `send_message`;
daemon notices influenced by agents must use Archon triggers. Stage relays
and refusal notices then remain non-authoritative.

A failed authority operation can have an ambiguous outcome when an external
write succeeded but local persistence or audit failed. Confirmations stay in
`Executing` to prevent replay: inspect the affected state, answer `no` to close
the question, then request a new change if needed. Closing it undoes nothing.
Likewise, a steer combining autonomy with other fields applies those fields
before autonomy; a later refusal may leave the earlier steer applied. Send
an autonomy-only change when that distinction matters. Repository removal
currently leaves an inactive overlay keyed to its former unique repository ID.
