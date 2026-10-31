# Get started

This walk-through sets Archon up and runs your first work almost entirely
from the chat. You talk to the orchestrator in plain words; it does the
work, and when something needs your yes it asks a numbered question that
you answer with `/answer <n> <answer>`.

The replies below show the shape of each exchange. The orchestrator words
its replies itself, so yours will read differently.

- [1. Install and open the chat](#1-install-and-open-the-chat)
- [2. Register a repository](#2-register-a-repository)
- [3. Repository settings](#3-repository-settings)
- [4. Queue work, watch it, merge it](#4-queue-work-watch-it-merge-it)
- [5. Standing directives](#5-standing-directives)
- [6. Lavish review pages](#6-lavish-review-pages)
- [7. GitHub, questions and lessons](#7-github-questions-and-lessons)
- [Chat commands at a glance](#chat-commands-at-a-glance)

## 1. Install and open the chat

```sh
curl -fsSL https://raw.githubusercontent.com/mengsig/archon/main/install.sh | sh
claude            # log in to your engine once (or use Codex)
cd ~/code/foo
archon
```

`archon` opens the terminal UI on the orchestrator. On the first run it
offers, one yes/no question at a time, to set up Archon's own Aether, to
register the checkout you are in and to run `archon doctor`. Prefer a plain
line chat (over SSH, say)? Use `archon chat`. `/quit` leaves either one and
the work keeps running. Prerequisites and other install methods are in
[Installing Archon](install.md).

## 2. Register a repository

Archon works only on repositories you register. Ask for it:

```text
you:     Register ~/code/foo as foo, ship pull requests, base branch main.
archon:  Registered foo (/home/you/code/foo): ship pr, base branch main.
```

The orchestrator registers a repository only when you ask, with its name,
ship mode (`pr` pushes and opens a pull request, `local` keeps commits on a
local branch), base branch and default pipeline. `/repos` lists what is
registered. Two things stay yours alone: letting a repository merge on its
own, and watching its GitHub issues (`/watch`, see
[section 7](#7-github-questions-and-lessons)).

## 3. Repository settings

Ask for a change in plain words. Settings are yours, so archond does not
take the orchestrator's word for them: a new base branch or default
pipeline, `ship` from `local` to `pr`, or a higher autonomy ceiling waits
for your answer to a numbered question.

```text
you:     For foo, use base branch next and the adaptive pipeline by default.
archon:  Q3: Approve operator authority change?
         Before: …
         Requested change: …
         Reply yes or no, or /answer N yes.
you:     /answer 3 yes
archon:  Applied: foo now forks from next, default pipeline adaptive.
```

The same goes for autonomy, the five knobs (review, proposal, report,
merge, tools) each item carries under a ceiling only you can raise:

```text
you:     Let foo merge its own pull requests once checks pass.
archon:  Q4: Approve operator authority change? …
you:     /answer 4 yes
```

A bare `yes` also works when it is the first message you send after the
question and only one confirmation is waiting; otherwise use
`/answer <n> yes`. Every change lands in the Authority feed with a number,
and a settings or autonomy change there can be reverted with
`/archon undo <n>`. If you would rather type the exact
change, a whole-message command applies at once, with no question:

```text
/archon set --repo foo base_branch=next
```

[Operator authority](operator-authority.md) lists every setting, what asks
first and the `/archon` command for each. `archon settings` and
`archon repo set` do the same from a shell.

## 4. Queue work, watch it, merge it

Say what you want done. Every change becomes a queue item (`qi_…`) that
runs through a pipeline in its own git worktree:

```text
you:     In foo, fix issue #12 and add a --dry-run flag to the export command.
archon:  Queued 2 items on direct:
         qi_01J… fix issue #12
         qi_01K… add --dry-run to export
```

Then watch it without asking the orchestrator; archond answers these
commands itself:

| Type | To see |
| --- | --- |
| `/status` | the daemon, Aether, the queue and spend |
| `/queue` or `/queue running failed` | the queue, optionally only some states |
| `/item qi_01J` | one item (a unique start of its id is enough) |

The orchestrator tells you when an item finishes, with its pull request.
To merge it at the commit the reviewers read:

```text
you:     /merge qi_01J
archon:  Applied: merged qi_01J…'s pull request.
```

`Merge the issue #12 fix` in plain words works too: the orchestrator asks
archond to merge, and archond asks you to confirm with a numbered question
unless the repository grants merges. With `ship = local`, `/diff` shows the
change and merging fast-forwards the base branch instead. To change a
running item, just say so ("in qi_01J, also update the docs"); see
[Steering an item](guide.md#steering-an-item).

## 5. Standing directives

An instruction that should outlast the conversation becomes a directive.
Start it with "from now on", or give it a time frame:

```text
you:     From now on, review anything that touches foo's auth/ strictly.
archon:  Recorded directive dir_01…: review anything that touches auth/ strictly.
```

Every future orchestrator reads directives, they survive restarts, and they
show in `archon status`. A directive never raises the autonomy ceiling. To
see or withdraw them:

```text
you:     Which directives are in force?
you:     Drop the directive about auth/.
```

From a shell, `archon autonomy` lists them, `archon autonomy direct "…"`
adds one and `archon autonomy clear <id>` (or `--all`) withdraws them.

## 6. Lavish review pages

[Lavish](https://github.com/kunchenguid/lavish-axi) (`lavish-axi`) turns an
HTML page into a review page in your browser, where you point at a part of
the page and leave a comment. It is optional:

```sh
npm install -g lavish-axi     # needs Node 22 or newer
```

Archon uses it whenever there is something to read before going on: a plan
when an item asks for a proposal first, a change report, an `e2e-testing`
or `design-doc` report, or a page you ask for:

```text
you:     In foo, plan the export rewrite and show me the proposal before building it.
archon:  Queued qi_01M… with a proposal first. Q5: the proposal waits for your
         decision. Page: http://127.0.0.1:…
```

Open the link and comment on the page; your comments go back to the
orchestrator or, on a waiting proposal, decide it. You can also answer the
gate in the chat: `/answer 5 approve`, `/answer 5 <feedback>` to send it
back, or `/answer 5 reject` to cancel the item. `archon reports` lists every
page with its id, and `archon reports reopen <id>` shows one again. Without
Lavish the same questions arrive in the chat with the file's path.

## 7. GitHub, questions and lessons

- **GitHub.** `/watch foo` watches foo's GitHub `origin`: mention `@archon`
  in an issue or pull request and the request reaches the orchestrator,
  which answers on the thread. It needs `gh auth login`, and only you can
  send it. The terminal UI's `/` menu does not list it, but it works there
  as in `archon chat`. See the [GitHub watcher](github-watcher.md).
- **Questions.** `/questions` lists everything that waits for you: gates,
  confirmations and the orchestrator's own questions. Answer any of them with
  `/answer <n> <answer>`; a bare option number picks that option. Gates
  also take `/approve [n]` and `/reject [n] [reason]`.
- **Lessons.** Finished runs leave short lessons that later agents read as
  advice. `/lessons` lists the proposed ones; accept or retract one with
  `archon lessons accept <id>` or `archon lessons retract <id>`. See
  [What Archon learns](guide.md#what-archon-learns).

## Chat commands at a glance

Type `/` in the terminal UI for the menu. Each command must be the whole
message; anything else is plain text for the orchestrator.

| Command | Does |
| --- | --- |
| `/answer <n> <answer>` | answer question `n` |
| `/questions` | list open questions |
| `/approve [n] [note]`, `/reject [n] [reason]` | decide a proposal or report gate |
| `/status`, `/queue [state …]`, `/item <id>`, `/repos` | read the queue and the daemon |
| `/merge <id> [--ignore-checks]` | merge an item's pull request now |
| `/lessons` | proposed lessons |
| `/archon <command>` | change an operator setting directly ([Operator authority](operator-authority.md#commands)) |
| `/watch <repository>` | watch a repository's GitHub issues |
| `/update` | update Archon and Aether once running agents finish |

`archon chat` adds `/pause`, `/resume`, `/diff`, `/image <path>`, `/help` and
more; the [user guide](guide.md#use) has the rest.
