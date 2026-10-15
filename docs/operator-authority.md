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
reached today.

| Control | CLI | Chat / TUI | GitHub | `request_authority` | Confirmation |
| --- | --- | --- | --- | --- | --- |
| Answer a question or gate (`answer`) | `archon answer` | `/answer`, `/a`, `/approve`, `/reject` | same | proposal/report gates only | none for gates; authority and merge confirmations are answered only by your own reply |
| Merge an item (`merge_item`) | `archon merge` | `/merge ITEM [--ignore-checks]` | same | no (the orchestrator has `request_merge`, a question you answer) | — |
| Repository ceiling `autonomy_max` | `archon repo set --autonomy-max` | `/archon set --repo R max.K=V` | same | `repo_update` | when it raises any knob |
| `grant_merge` | `archon repo set --grant-merge` | `/archon set --repo R grant_merge=true` | same | `repo_update` | when set to true |
| `ship`, `base_branch`, `default_pipeline` | `archon repo set` | `/archon set --repo R ship=pr …` | same | `repo_update` | `ship` local→pr, and any `base_branch` or `default_pipeline` change |
| Caps and budgets (`set_caps`) | `archon queue caps` | `/archon apply {…}` | same | `set_caps` | when any limit rises, is cleared (zero) or budgets are turned off |
| Hard pause | `archon pause --hard` | `/archon pause --hard` | same | `pause` | no |
| Hard resume | `archon resume --hard` | `/archon resume --hard` | same | `resume` | always |
| Register a repository with `auto_merge` (`repo_add`) | `archon repo add` | `/archon apply` | same | `repo_add` | always |
| Remove a repository | `archon repo remove` | `/archon repo remove R` | same | `repo_remove` | always |
| Directives (`set_directive`, `clear_directive`) | `archon autonomy direct` / `clear` | `/archon apply` | same | yes | `clear_directive` only |
| Lessons and learning corrections | `archon learning …` | `/archon apply` | same | `update_lesson`, `learning_*` | always |
| GitHub watcher configuration | `archon github-watcher` | `/archon watcher JSON` | **refused** | `configure_github_watcher`, operator chat only | always |
| Undo an authority change | — | `/archon undo N` | same | no | guarded by unchanged state |
| Say something as the operator (`say`) | `archon say` | — | — | no | — |
| Restart / attach the orchestrator | `archon orchestrator restart`, `attach` | not implemented | not implemented | no | — |
| Shutdown, setup, update, destructive GC | CLI only | no | no | no | — |

The confirmation column applies to plain-language requests relayed through
`request_authority`. Explicit whole-message commands apply directly, since
the operator has named the exact action. Tightening (lower ceiling, hard pause, lower caps) applies
directly. An action whose direction cannot be ordered safely is treated as
loosening.

## Commands

A message that is exactly one of these is applied by archond. On GitHub put
`@archon` first. Anything else in the message makes it ordinary text.

```text
/answer N <answer>            /a N <answer>
/approve [N]                  /reject [N] [reason]
/merge ITEM [--ignore-checks]
/archon set --repo NAME max.merge=auto grant_merge=true ship=pr base_branch=main default_pipeline=adaptive
/archon pause --hard          /archon resume --hard
/archon repo remove NAME
/archon watcher <change JSON>        # operator chat only
/archon apply {"cmd": …, …}          # any allowlisted action, as JSON
/archon undo N                       # N from the Authority feed
```

Details:

- `/archon set --repo` takes `max.<knob>=<value>` (the same values as
  `archon autonomy`), `grant_merge`, `ship`, `base_branch`,
  `default_pipeline`. It replaces that repository's whole ceiling override
  with the current one plus your change, so other knobs stay as they were.
- `/archon apply` accepts only a finite allowlist: repository add, update and
  remove; caps; pause and resume; directives; lessons and learning
  corrections; watcher configuration. Shutdown, setup, update, destructive
  GC, nested authority requests and everything else are refused.
- A command is limited to 4000 characters.
- Caps set this way, like `archon queue caps`, last until archond restarts.
- Explicit commands apply directly, including loosening. The GitHub
  `github_may_loosen` switch and watcher exclusion still apply.
- `archon chat` preserves a leading `//` escape when forwarding literal text
  to Aether, so the delivered message cannot become a command.

## Delegated authority: `request_authority`

When you say it in prose ("let `web` merge on its own"), the orchestrator
calls `request_authority` with:

- `utterance`: the inbox sequence of your message;
- `quote`: text that occurs verbatim in that message;
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
5. The quote occurs verbatim in it.
6. The action is on the allowlist, and for GitHub is not a watcher change.
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

- `/answer N yes` or `/a N no` in the same channel, or `archon answer`.
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

A repository setting or a caps change that was applied ends with
`undo with /archon undo N`. Undo restores the previous repository settings
(ceiling, `grant_merge`, `ship`, base branch, default pipeline) or caps. It
is refused if those settings have changed since: it never overwrites a newer
change. A cited undo asks when it restores a looser value; an explicit
`/archon undo` applies directly. Undo is not offered for pause, directives,
removals or watcher changes.

GitHub replies never include private settings, spend or other repositories.

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

- **Archon protocol 12** adds daemon-verified authority requests
  (`request_authority`, `undo_authority`) and the `guide` request. Older
  archond versions do not know them.
- **Aether protocol 17** or newer is needed for GitHub operator standing,
  because the watcher's envelope flag is read from it. On an older Aether,
  GitHub operator commands and citations are not recognised. Chat authority
  needs no more than before.

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
- Shutdown, setup, update and destructive GC are CLI only and have no chat
  or GitHub form.
- `request_authority` depends on the orchestrator following its
  instructions to cite and to restate. archond enforces the checks above
  regardless, but cannot make the orchestrator ask.
- A GitHub operator is as powerful as your terminal for the allowlisted
  actions above. List only accounts you would hand it to
  ([Operator logins](github-watcher.md#operator-logins)).

Planned, **not shipped** (pending the parent change set): a persistent
global and per-repository operator settings overlay with a `settings` CLI;
`update_item` / `steer_item` autonomy for the orchestrator; and automatic
merging of a shipped item at its reviewed head. This page will describe them
when they land.

Prompt budgets for this feature: core orchestrator prompt ≤ 12 KiB, each
guide topic ≤ 4 KiB, the MCP tool list ≤ 28 KiB, a new tool ≤ 1.5 KiB.
