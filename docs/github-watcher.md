# GitHub watcher

Tag `@archon` in an issue, pull request, comment or submitted review on a
watched repository, and the request reaches Archon's orchestrator. The
orchestrator can answer on the thread, research with the `ask` pipeline, and
queue work into the matching registered repository. When that work finishes,
it posts the outcome back to the same thread.

Archon does not poll GitHub itself. It reuses the GitHub watcher of
**Archon's own aetherd** (dedicated mode) and points it at the orchestrator
in Aether's **fixed** dispatch mode. Aether decides who may ask, and
whether a request carries your authority (off unless you list
[operator logins](#operator-logins)); Archon decides where requests go and
what a reply may touch. The operator's own
Aether daemon is never read or changed. In shared mode every
`archon github-watcher` command is refused.

## Setup

1. Install `gh` where archond and Archon's aetherd run, and log in with
   `gh auth login`. The token needs read access to the repository and to
   collaborator permissions, and the right to comment. Both daemons run as
   you and use the same `gh` login. Archon never stores a token.
   `archon doctor` runs `gh auth status` and fails while it does not pass
   and the watcher is on or a repository ships in `pr` mode.
2. Watch a repository:

   ```sh
   archon github-watcher add owner/repo
   archon github-watcher            # settings, targets and polling health
   ```

   For a registered repository, `archon repo add <path> --github` (or
   `/watch <name>` in the orchestrator's chat) watches its `origin`
   the same way; a repository whose origin is not on github.com is then not
   registered. Agents cannot turn watching on: `register_repo` never does.

   `add` needs the orchestrator running (`archon status`), because it aims
   the repository at it. The first `add` also switches the watcher on and
   replaces Aether's default `@aether` patterns with Archon's:

   | Setting | Default |
   | --- | --- |
   | Mention | `(?i)(?:^|[^a-z0-9_@])@archon(?:$|[^a-z0-9_-])` matches `@archon` and `@Archon`, but not `@archon-bot` or `me@archon` |
   | Label | `(?i)archon`, any added label containing `archon` |

   Patterns you have already changed stay as they are.
3. Optionally, narrow who may dispatch (see
   [Who may dispatch](#who-may-dispatch)):

   ```sh
   archon github-watcher allow owner/repo alice bob
   ```
4. Optionally, let your own GitHub login instruct the orchestrator with
   your authority. Read [Operator logins](#operator-logins) first: a listed
   account is as powerful as you are.

   ```sh
   archon github-watcher operators alice
   ```

   On an installation upgraded from an older Archon, restart the
   orchestrator first: archond refuses to set the list while the
   orchestrator that takes GitHub requests runs older instructions (see
   [Upgrading](#upgrading-an-existing-installation)).
5. Register the local checkout with `archon repo add <path>`, so the
   orchestrator can queue work for it. It matches a GitHub repository to a
   registered one by the checkout's `origin` remote.

### Upgrading an existing installation

An orchestrator started by an older Archon lacks two things: the GitHub
policy in its instructions, and the new tools (`github_reply`, `enqueue`'s
`origin`).

- **Instructions.** archond hands GitHub requests only to an orchestrator
  started with this build's GitHub section in its instructions. It never
  patches them into a live one: a turn keeps the instructions it started
  with, and Aether steers new inbox items, GitHub requests included, into a
  running turn. So `add`, `enable` and the repoint check refuse any write
  that would start delivering to an orchestrator without the policy, and
  say to run `archon orchestrator restart`. Meanwhile `configure`,
  `remove` and `disable` still apply, and leave the other repositories'
  targets where they were. For the same reason `operators <LOGIN>...` is
  refused while a repository hands requests to such an orchestrator;
  `operators --clear` always applies.
- **The first watched repository.** To save every turn its cost, an
  orchestrator gets the GitHub section only once the watcher is wanted:
  when Aether's watcher holds a repository as it is created, or after an
  `add` or `enable` was asked for. So the first `add` on an installation
  that never watched may also say to restart; after the restart it applies.
- **Tools.** An agent keeps the tool list it started with, and Archon's
  aetherd keeps running the old `archon mcp` server. Once, after upgrading,
  run:

  ```sh
  archon aether mcp refresh archon
  archon orchestrator restart
  ```
- **Aether protocol 17.** Archon speaks Aether protocols 17 to 24, so every
  aetherd it connects to stores allowlists and operator logins. An aetherd
  older than 17 is refused at connect, and `archon doctor` fails naming
  both ranges: install a newer `aether`, then run `archon daemon stop
  --aether` and `archon daemon start`.
- **Operator logins** come last, after the tools and the orchestrator:

  1. **The tools and the orchestrator.**

     ```sh
     archon aether mcp refresh archon
     archon orchestrator restart
     ```

     The restarted orchestrator runs this build's instructions, which have
     the rules for operator standing. They are harmless while no list is
     set: every request is still external.
  2. **Then the list.** `archon github-watcher operators <LOGIN>...`

     archond refuses this step until step 1 is done, and says so: while a
     watched repository hands requests to an orchestrator started before
     it, nothing is written and the error says to run
     `archon orchestrator restart` and set the logins again.

  The order matters because Aether marks a listed login's requests as your
  instructions from the moment the list is set, and an orchestrator started
  before the upgrade keeps instructions that call every GitHub request
  external and say nothing of operator standing.
- **archond.** The `archon` CLI and archond speak Archon's protocol 3 now,
  so a CLI and an archond from either side of this change refuse each other
  at hello. After upgrading, restart archond (`archon daemon stop --keep-aether`, then
  `archon daemon start`), then reload the `archon mcp` server and restart
  the orchestrator:

  ```sh
  archon daemon stop --keep-aether
  archon aether mcp refresh archon
  archon orchestrator restart
  ```

  Until then the orchestrator has no Archon tools: the `archon mcp`
  server aetherd started before the upgrade speaks protocol 2, which the
  new archond refuses.

## Commands

| Command | What it does |
| --- | --- |
| `archon github-watcher [status\|list]` | revision, patterns, interval, the orchestrator, the operator logins (or that none are set), each repository's target, allowlist and health, and each operator login a repository's allowlist leaves out |
| `archon github-watcher add <owner/name> [--allow LOGIN]...` | watch a repository, aimed at the orchestrator; switches the watcher on. `--allow` sets its allowlist in the same write |
| `archon github-watcher allow <owner/name> <LOGIN>...` | replace a watched repository's allowlist |
| `archon github-watcher allow <owner/name> --clear` | remove it: any collaborator with write access may dispatch again |
| `archon github-watcher operators <LOGIN>...` | replace the [operator logins](#operator-logins): GitHub accounts whose own requests are your instructions. Prints a warning. Refused while a repository hands requests to an orchestrator started without this build's GitHub instructions |
| `archon github-watcher operators --clear` | remove the list: every GitHub request is an external request again (the default). Never waits for the orchestrator |
| `archon github-watcher remove <owner/name>` | stop watching it |
| `archon github-watcher enable\|disable [<owner/name>]` | one repository, or the whole watcher |
| `archon github-watcher configure [--mention-regex R] [--label-regex R] [--poll-interval SECS]` | what matches and how often Aether polls (60 to 86400 s) |

Every edit is a compare-and-swap on Aether's config revision. If someone
else changed the watcher since archond read it, for example through
`archon aether github-watcher`, the edit fails with "refresh before
editing" and nothing is overwritten. Run the command again.

No edit drops a set list of operator logins: `add`, `allow`, `remove`,
`enable`, `disable`, `configure` and archond's own repointing all write back
the list they read, and archond reports an error if Aether answers a write
with a different one. Only `operators` changes it.

Once a repository is watched, `archon status` and the orchestrator's
`queue_status` show a `github:` line. `archon status` also names the
operator logins there when any are set.
A repository still aimed at an old orchestrator is flagged as a
**MISMATCH**. archond repoints Archon's repositories whenever the
orchestrator is replaced, including a restart by `archon orchestrator
restart`. It also checks once a minute and at startup, so a crash between
the restart and the repoint heals by itself. A repointed repository whose
new orchestrator is paused is refused by Aether and is retried. An enabled
repository is never repointed to an orchestrator started without the GitHub
policy (see [Upgrading](#upgrading-an-existing-installation)); it stays a
mismatch until `archon orchestrator restart`.

## Who may dispatch

By default Aether delivers a request from any human collaborator with
`admin`, `maintain` or `write` permission. A repository's allowlist narrows
that to named GitHub logins. To let only `alice` and
`bob` dispatch on `example-org/example-repo`:

```sh
archon github-watcher add example-org/example-repo --allow alice --allow bob
# or, for a repository already watched:
archon github-watcher allow example-org/example-repo alice bob
```

`--allow` is repeatable and takes comma-separated logins too
(`--allow alice,bob`). On `add` it is saved in the same
configuration write as the repository, so the repository is never watched
without its list. `add` without `--allow` keeps a watched repository's list.
`allow` changes only the list: it enables and retargets nothing, and works
on any watched repository, Archon's or not.

```text
example-org/example-repo  on  -> orchestrator  only alice, bob  polled 1m ago, 3 dispatched, 1 ignored
```

- The allowlist narrows Aether's permission check and never replaces it. A
  listed login still needs write access, so removing someone's access on
  GitHub revokes them even while they stay listed.
- Anyone not listed is ignored like any unauthorized requester: nothing is
  delivered and the repository's `ignored` count goes up. For a label, the
  check applies to whoever applied it.
- Logins match case-insensitively and are stored lowercase without repeats.
  Each must be a GitHub login: 1 to 39 letters, digits or single hyphens,
  not starting or ending with a hyphen. At most 100 distinct logins fit one
  repository. The CLI checks these rules before sending, and an empty list
  is refused: `allow --clear` removes the list instead.
- The allowlist is Aether's `allowed_users` field.

Operator logins never widen any of this. A login you list as an operator
still needs write access, and a place on the repository's allowlist where
there is one. If the allowlist leaves it out, its requests are not delivered
there at all, with or without authority, and `status` says so.

Repositories that use Aether's **automatic** or **thread** dispatch on
Archon's aetherd are not Archon's. They are listed as such, never repointed,
and `github_reply` refuses their threads. `add` refuses to take one over;
`remove` it first.

## Operator logins

By default every GitHub request is an **external request**: the
orchestrator helps within what you have already allowed, and refuses to
change how Archon runs, whoever asked. Operator logins change that for the
GitHub accounts you name. A request that one of them wrote reaches the
orchestrator as your own instruction, with the authority of a message you
type in the terminal UI or `archon chat`.

```sh
archon github-watcher operators alice bob                 # replaces the list
archon github-watcher operators --clear             # nobody again
archon github-watcher status                        # shows the list
```

> **A listed account is as powerful as you are at your terminal.** Whoever
> controls it, or a token that can post as it, can direct Archon with your
> authority on every watched repository where that login may dispatch:
> record directives, register repositories, pause and resume, and queue
> work with any knob the autonomy ceiling allows, `merge: auto` and
> `tools: yolo` included. Losing the account is losing that authority. List
> only accounts you would hand your terminal to, and clear the list the
> moment one is in doubt. The list holds login names, not account ids:
> remove a login before its account is renamed or deleted, because someone
> else can then take the name.

The list is empty until you set it. Setting one prints that warning once.
Logins follow the allowlist's rules: GitHub logins only, matched without
regard to case, stored lowercase without repeats, at most 100, and an empty
list is refused (`--clear` removes it). The CLI checks them before anything
is sent.

archond sets a list only once the orchestrator knows what it means. Every
orchestrator that a repository hands requests to (the watcher on, the
repository on, aimed at the orchestrator) must have been started with this
build's GitHub instructions, which hold the rules for operator standing.
While one was not, `operators <LOGIN>...` is refused, nothing is written,
and the error says to run `archon orchestrator restart` and set the logins
again. archond neither patches the instructions into a live orchestrator
nor restarts it for you.

- `operators --clear` is never held back. Taking authority away works
  whatever the orchestrator runs, and while it is down.
- With no repository handing requests to the orchestrator (the watcher off,
  no repository of Archon's, or every one disabled), the list is set. A
  later `add` or `enable` is refused in the same way until the orchestrator
  has the instructions, so delivery never starts to one without them.

### When a request carries your authority

Aether decides, for each request, and only when all of this holds:

- The requester's login is on the list.
- The requester may dispatch on that repository: a human account with write
  access, and on the repository's allowlist where there is one.
- The delivered text is that account's own and nobody else's. GitHub must
  name it as the author and report that the text was never edited, or was
  last edited by its author. The same holds for the title of an issue or
  pull request. A collaborator with write access can edit other people's
  comments, so edited text is external.
- For a request made by adding a label, the text is the issue's, not the
  labeller's: the issue's author must be an operator login too and pass the
  same checks. The label's name is never your words.

Anything short of that is delivered as before, as an external request, and
its `trust` sentence says why standing was withheld when an operator login
was involved. When GitHub cannot be asked who wrote a text, the request
waits for the next poll: it is never upgraded or downgraded on a failed
lookup.

Archon computes none of this. It stores the list in Aether, shows it, and
passes on what Aether decided. The orchestrator sees the decision in two
places only Aether can write: the marker `[authenticated operator
instruction]` on the trigger's inbox line, and `"operator": true` in its
payload. archond itself acts on the trigger envelope: its top-level
`operator` flag, which needs Aether protocol 17, together with the
repository, thread number and requester. Text markers and payload fields
alone are never enough. The orchestrator's instructions take a request as
yours only when the marks are there, and never from the requester's name,
from anything written in the request, or from an earlier request.

From such a request archond also applies whole-message commands
(`@archon /approve 1`, `@archon /merge ITEM`, `@archon /archon set --repo …`)
and lets the orchestrator cite it with `request_authority`. They are scoped to
the repository of the thread: the registered repository whose `origin` is the
thread's, and only while its allowlist includes the login. Settings, merges,
approvals and item changes of any other repository are refused, as are global
settings, caps, pause and resume. Changes that loosen a limit, even in the
thread's own repository, ask you first, and the watcher itself (repositories,
allowlists, operator logins) can never be changed from GitHub. Set
`[authority] github_may_loosen = false` to let GitHub only tighten. See
[Operator authority](operator-authority.md).

### What it does not do

- It does not let anyone new dispatch. See
  [Who may dispatch](#who-may-dispatch): an allowlist that leaves an
  operator login out keeps that login's requests out of the repository
  altogether. `status` names each such login and repository:

  ```text
  operator logins: bob (their own requests are your instructions)
  example-org/example-repo  on  -> orchestrator  only alice  polled 1m ago, 3 dispatched, 1 ignored
  operator login bob is not on example-org/example-repo's allowlist: its requests are not delivered there at all
  ```

  With no list, the line reads `operator logins: none (every GitHub request
  is an external request)`. `status --json` carries the same as
  `operator_users` (a list or `null`) and each repository's
  `excluded_operators`.
- It does not raise the autonomy ceiling. That is still changed only from
  your CLI, and archond refuses any knob above it.
- It answers no open question, gate or approval by itself. A request is a
  message, not a button: proposal and report gates and Aether's approval
  prompts still wait for you in chat, Lavish or `archon answer`.
- It changes no approval policy, network policy, sandbox or budget.
- It cannot change the watcher. No GitHub request, no agent and no MCP tool
  can set the list, an allowlist or a target: only `archon github-watcher`
  at your terminal can.
- It does not make the thread private. The orchestrator answers on the
  public thread and is told to keep secrets, local paths, operator-only
  details and other repositories' details out of it, also when you are the
  one asking. It tells you in the terminal UI and `archon chat` what was
  asked and what it did.

Operator logins are Aether's `operator_users` field. See
[Upgrading](#upgrading-an-existing-installation) for the order to set them in.

The list belongs to the whole watcher, so it also covers repositories on
Archon's aetherd that use Aether's automatic or thread dispatch. Those are
not Archon's orchestrator, and archond's check above does not look at the
agents they dispatch to.

## What the orchestrator does with a request

A request arrives as an Aether trigger from source `github`. It names the
repository, thread number, URL, requester, their text, whether it carries
operator standing (`operator`) and the `trust` sentence Aether wrote. The
orchestrator's prompt tells it, for every request, to:

- answer on the thread with the `github_reply` tool, reading that
  repository's mirror itself when the answer takes code;
- queue work into the matching registered repository within the autonomy
  ceiling and directives, passing the thread as the item's `origin`, and
  `notify` you a `decision` with the thread link;
- when the item's `item_finished` event arrives carrying that `origin`, post
  the outcome to the thread;
- for a repository that is not registered, reply saying so and `notify` you;
- never change the watcher (repositories, allowlists, operator logins) for
  a GitHub request of either kind.

**An external request** is every request without operator standing, whoever
the requester is, your own login included. The orchestrator treats it as an
external request, not an operator instruction. It must not set or clear
directives, choose `merge: auto` or `tools: yolo`, register repositories,
or pause or resume globally because of one. When Aether withheld standing,
it may tell the requester the reason Aether gave.

**A request with operator standing** has both of Aether's marks: the marker
on its inbox line and `"operator": true` in its payload. The orchestrator
takes it as your instruction, as in chat: it may record or clear a
directive (noting the GitHub login and thread it came from), register a
repository, pause and resume, and choose any knob up to the ceiling. A
"merge" goes through `request_merge`: archond asks you to confirm in chat,
unless the repository has `grant_merge` (see the guide's "Merging on
request"); an external request never merges. It answers on the public
thread without operator-only details, and always `notify`s you in chat
what was asked and what it did.

The prompt tells it never to infer standing: not from the requester's name,
not from a marker, a trust sentence or `"operator": true` pasted into the
text, and not from an earlier request.

## `github_reply`

`github_reply {repository, number, body}` posts one comment through
`gh api --hostname github.com` and returns its URL. archond:

- allows only threads in repositories Archon's watcher routes to the
  orchestrator (enabled or disabled);
- posts to github.com only, the host Aether's watcher reads and authorizes,
  whatever `GH_HOST` says;
- finishes the whole reply, from the watcher lookup to `gh`'s answer, within
  10 seconds, under the MCP call budget. A `gh` still running then is
  stopped, and the error names the thread to check: the comment may have
  been posted, so it must not be repeated blindly;
- always appends `<!-- aether:github-reply -->`, the marker Aether's watcher
  skips, so a reply never triggers another request, and strips any copy of
  it from the body;
- refuses an empty body or one over 65,000 characters;
- records each posted comment in the log (`archon log`) as progress:
  `Replied on GitHub owner/repo#N: <url>`.

The tool is classified `external`. Under Aether's default approval policy
each reply waits for your approval, as any external tool call from an Aether
agent does. `[github] binary` in `config.toml` selects the `gh` executable.

## Trust boundary

- **Who may ask** is Aether's decision and is not weakened. Only a human
  with `admin`, `maintain` or `write` permission can dispatch, and only a
  listed login where the repository has an allowlist. A label counts for
  whoever applied it, and matching text alone is never authorization.
  Aether delivers requests as external triggers, never as operator
  messages.
- **Who speaks as you** is Aether's decision too, and nobody does unless
  you list operator logins. Aether marks a request as yours only after
  checking the login, its right to dispatch and that it alone wrote the
  text ([Operator logins](#operator-logins)). Archon never computes
  standing and never adds to it: it passes on Aether's two marks, and the
  prompt forbids reading standing from anything else.
- **What the watcher does** is changed only by the operator. Watcher edits,
  the operator logins included, are operator-only requests on archond's
  socket: the orchestrator's MCP client is refused, no MCP tool builds one
  (`queue_status` only reads a summary), and Aether accepts watcher
  configuration only from its local user socket. Nothing on GitHub can add a repository,
  retarget one, change its allowlist or name an operator login, with or
  without operator standing.
- **What a request can cause** is bounded by archond, not by the request.
  The autonomy ceiling, caps, budgets and model allowlist apply to every
  item. Raising a ceiling, a hard pause and restarting the orchestrator
  stay operator-only. Replies go only to Archon's watched repositories.
- The prompt rules above (no directives, no `merge: auto` or `tools: yolo`,
  no registration, no global pause for an external request; standing only
  from Aether's two marks) are instructions to the model, not checks in
  archond: the orchestrator holds those tools for your own requests. Keep
  the ceiling at what you would accept from any collaborator on a watched
  repository.
- The orchestrator and the stage agents run as your Unix user, as before.
  A shell that runs as you can run `archon` as you; the operator-only
  boundary is the MCP surface and the prompt, not the operating system.
- Collaborators can still send hostile text or code. Stage agents work in
  isolated worktrees under the item's `tools` knob. Keep `tools: yolo` and
  `merge: auto` out of the ceiling of repositories whose collaborators you
  do not trust.

## Limits

Everything Aether's watcher documents applies (`docs/github-watcher.md` in
Aether):

- GitHub.com only, at most 32 repositories.
- Polling starts from the moment a repository is enabled; there is no
  backfill.
- At most 20 deliveries per thread per hour.
- Bounded work per poll; errors back off and show in status.

The orchestrator has one inbox, so requests from many threads are handled
one after another, not in parallel. Each reply is a new comment; replies are
never edited.

With operator logins set:

- Standing belongs to one request. A later comment on the same thread
  earns it again or is external.
- Text edited by anyone but its author, and text whose editor GitHub does
  not name, is external.
- Aether reads authorship from GitHub's GraphQL API with the same `gh`
  login. A credential that cannot use it holds back a repository's requests
  whenever an operator login asks, until it can or the list is cleared; the
  error shows in `status`.
- One list covers the whole watcher. There is no list per repository; use a
  repository's allowlist to keep an operator login out of it.
