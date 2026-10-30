# Security policy

## Supported versions

| Version | Supported |
| --- | --- |
| 1.x | Yes. Fixes go into the latest 1.x release. |
| before 1.0 | No. |

## Reporting a vulnerability

Report it privately through GitHub: open the repository's **Security** tab
and choose **Report a vulnerability**, or go straight to
<https://github.com/mengsig/archon/security/advisories/new>.

Do not open a public issue, discussion or pull request for a suspected
vulnerability.

Include what you can of:

- the Archon version (`archon --version`), the Aether version and the
  operating system;
- what an attacker gains, and what they need to start from (a collaborator
  account on a watched repository, a file in a repository, a local account on
  the machine, ...);
- steps to reproduce, ideally with a minimal configuration and repository;
- whether the problem is already public anywhere.

## What to expect

A maintainer reads every report and answers in the advisory thread. Archon
has a small maintainer team, so there is no promised response time and no bug
bounty. For a confirmed vulnerability we fix it in the latest 1.x release,
publish the advisory, and credit you unless you ask us not to. If we decide a
report is not a vulnerability we say why.

Behaviour described under "Trust model" below is how Archon is designed to
work. A way around one of the limits it lists as enforced is a vulnerability.
An agent doing something its Unix user is allowed to do is not.

## Trust model

**Archon is not a sandbox.** It starts coding agents through Aether (Claude
Code, Codex and others), and those agents run as your Unix user. They have a
shell, your files, your network access and whatever credentials your user can
reach: `gh`, SSH keys, tokens in the environment or on disk.

Each work item runs in its own git worktree. That keeps agents out of your
checkout and keeps parallel items from editing each other's files. A worktree
is a directory, not a boundary: a process in it can read and write anything
your user can.

Archon's controls limit what Archon asks agents to do and what it lets them
start. They do not confine a process that ignores its instructions.

| Control | What it does | What it does not do |
| --- | --- | --- |
| **Autonomy ceiling** | Bounds five knobs per item: review depth, proposal first, report before the pull request, auto-merge, and `tools` (`guarded` or `yolo`). `archond` refuses an item above the ceiling. You raise it in two places: `[autonomy] max` in your own `config.toml`, for every repository, and `archon repo set --autonomy-max` (or `--auto-merge` on `archon repo add` and `archon repo set`) for one repository. No MCP tool raises it, and a repository's own `.archon/config.toml` can only lower it. The built-in ceiling allows everything except `merge = auto`. | It does not confine an agent's process. With `tools: yolo` nothing waits for approval and agents may delegate. Keep `yolo` and `merge: auto` out of the ceiling for repositories whose content or collaborators you do not trust. |
| **Model policy** | An allowlist of models and a maximum effort. `archond` refuses a stage outside it and cancels an agent found outside it. | It says which models may run, not what they do. A model on the list is as capable of a harmful command as any other. |
| **Budgets and caps** | Daily and total budgets stop new work from being dispatched once reached. An exhausted item budget fails that item's run and cancels its agents. Caps bound how many agents run at once. | They are best-effort. Spend is counted as it is reported, so a running turn can overshoot, and some providers report no cost, so the figure is an estimate. A budget is not a limit on your provider's bill. |
| **GitHub watcher: who may ask** | Only a human with `admin`, `maintain` or `write` permission on a watched repository can dispatch, and only a listed login where the repository has an allowlist. Such a request is an *external request*: the orchestrator is told to answer, research and queue work within the ceiling, and not to change how Archon runs. | Collaborators can still send hostile text or code. The rules for external requests are instructions to the model, not checks in `archond`. Set the ceiling to what you would accept from any collaborator on a watched repository. |
| **GitHub watcher: operator list** | It is empty by default. A login you add with `archon github-watcher operators` speaks with your authority, up to the ceiling, over the repository of the thread it writes in, where it may dispatch. It has no authority over other repositories or global settings, and a change that loosens a limit asks you first. Aether checks the login, its right to dispatch, and that it alone wrote the text. No GitHub request and no MCP tool can change the list. | It does not protect you from a listed account being taken over: whoever controls it, or a token that can post as it, directs Archon as you. It never raises the ceiling. |
| **Remote access (mobile web app)** | Off by default and bound to loopback. Without a paired device's credential (a 256-bit secret per device, stored as its SHA-256 in `remote.json`, mode 0600) the listener serves only the app's files, `GET /api/health` (whether it runs, its version and whether a pairing is open) and the pairing claim, which needs a one-time code shown on your terminal, open 5 minutes and rate-limited; the session and the WebSocket need the credential. Every request's `Host` must name an origin you allowed, and every `POST` and the WebSocket upgrade an allowed `Origin`, with `Sec-Fetch-Site: same-origin` when the browser sends it. A device may send only a fixed list of requests, handled as the operator CLI's; merges, promotions, settings, ceilings, gc, upgrades and remote access itself stay on the CLI. `archon remote revoke` and `disable` close live sessions at once. | The listener speaks plain HTTP: TLS and who may reach the address are the job of `tailscale serve` or your proxy. Whoever holds a paired phone, or its credential, can operate the queue and chat with the orchestrator as you. Details in [`docs/remote.md`](docs/remote.md#threat-model). |

The details are in [`docs/github-watcher.md`](docs/github-watcher.md)
("Trust boundary") and
[`docs/ARCHITECTURE.md` §18](docs/ARCHITECTURE.md#18-autonomy).

### Known limitation: an agent's shell can run the operator CLI

Operator-only actions (raising the autonomy ceiling, changing the watcher's
repositories, allowlists and operator list) are refused on the MCP surface
the orchestrator uses, and the prompts forbid them. That boundary is not the
operating system. The orchestrator and the stage agents have a shell as your
Unix user, the `archon` CLI talks to `archond` over a socket your user owns,
and the client kind is self-declared.

Aether marks every process it starts for an agent with `AETHER_ENGINE_RUN`
and `AETHER_ENGINE_AGENT`. An `archon` CLI that finds them sends its
requests for that agent, so `archond` refuses it operator-only commands
and lets only the orchestrator change Archon; with no agent named it sends
only reads. This stops an agent that simply runs `archon` in its shell, but
not one that unsets the variables first: such a shell is then accepted as
the CLI, with every operator-only command available to it, including adding
a GitHub login to the operator list. Your `config.toml` is likewise a file
that your user, and so an agent's shell, can write.

This is an open limitation, known before the first public release. The
fixes under consideration are a peer check in `archond` and a typed
confirmation at a terminal for the commands that grant authority.

Until then, treat anything that can steer an agent (a repository's files, an
issue or comment from a collaborator, a web page an agent reads) as able to
reach the operator CLI, and run Archon under a Unix user, or in a container
or virtual machine, whose access you are prepared to hand to the agents.
