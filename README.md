# Archon

Archon runs coding tasks through queues and review pipelines, using agents on the Aether daemon.

![Illustrated Archon overview: one orchestrator directs mascot agents through a persistent backlog and seventeen built-in default pipelines or your own TOML pipelines with stage prompts, with daemon guardrails, two ship modes and optional Lavish review pages, all running on Aether.](docs/assets/archon_graphic.svg)

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/mengsig/archon/main/install.sh | sh
```

Archon needs [Aether](https://github.com/mengsig/aether); the installer offers
to install it. See [prerequisites](#prerequisites) for the engine CLI and other programs.
Update with `archon update`. See [other installation methods](docs/install.md).
To build from source instead, you need git and Rust from [rustup](https://rustup.rs/); the repository pins its toolchain:

```sh
git clone https://github.com/mengsig/archon
cd archon
make install        # or: cargo install --locked --path crates/archon
```

The binary goes to `~/.cargo/bin`; update a source build with `git pull` and the same command, not `archon update`. Aether is installed separately.

## What you can do

- Describe work in chat, from the CLI, or through GitHub mentions on watched repositories.
- Queue tasks across repositories with priorities, dependencies, concurrency caps, and spending budgets.
- Run implementation, testing, and independent review stages, with fix loops when gates fail.
- Split work across parallel reviewers or team roles, each team role using its own worktree.
- Open pull requests or keep commits local.
- Read reports, answer questions, and review proposals in Lavish or through the CLI.

## Prerequisites

Use Linux or macOS (macOS is not covered by CI).
Install Aether and at least one engine CLI, then log in
with that engine. The default orchestrator uses Claude Code; see
[model configuration](docs/configuration.md#select-an-available-model) to use Codex instead.
The table's `brew` commands work with [Homebrew](https://brew.sh) on either OS.

| Program | Used for | Required? | Install |
| --- | --- | --- | --- |
| [Aether](https://github.com/mengsig/aether) | Runs the orchestrator and stage agents | Yes | The Archon installer offers installation; see Aether's installation instructions. |
| [git](https://git-scm.com/install/) | Repository branches, worktrees, commits | Yes for repository work | `brew install git` |
| [Claude Code](https://code.claude.com/docs/en/quickstart) (`claude`) | Claude engine and its login | One engine required | `curl -fsSL https://claude.ai/install.sh \| bash` |
| [Codex](https://github.com/openai/codex) (`codex`) | Codex engine and its login | Alternative engine | `npm install -g @openai/codex` |
| [GitHub CLI](https://cli.github.com/) (`gh`) | PR shipping and watcher replies | Optional; local work works without it | `brew install gh`, then `gh auth login` |
| [Node.js / npm](https://nodejs.org/en/download) | Runs Lavish; installs npm CLIs; `npm audit` for JS dependencies | Optional with native Claude and no Lavish | `brew install node` (Node 22+ for Lavish) |
| [lavish-axi](https://github.com/kunchenguid/lavish-axi) | Annotated HTML review pages | Optional; HTML files and CLI answers still work | `npm install -g lavish-axi` |
| [Rust / Cargo](https://rustup.rs/) | Builds Archon from source | Optional for binary installs | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| [Playwright](https://playwright.dev/docs/intro) | Browser journeys in web testing pipelines | Optional; use another browser driver or skip web testing | `npm init playwright@latest` in the tested project |
| [cargo-audit](https://github.com/rustsec/rustsec/tree/main/cargo-audit) | Rust dependency audit in `dependency-upgrade` | Optional; unavailable audits are reported | `cargo install cargo-audit --locked` |
| [pip-audit](https://pypi.org/project/pip-audit/) | Python dependency audit in `dependency-upgrade` | Optional; unavailable audits are reported | `python -m pip install pip-audit` in a virtual environment |

Install prerequisites for the default PR-and-review-page workflow on Linux or
macOS with Homebrew already installed; the Archon installer offers Aether. Add project-specific tools as needed:

```sh
brew install git gh node curl
npm install -g lavish-axi
curl -fsSL https://claude.ai/install.sh | bash
```

## Quick start

Run `claude` once to log in, then exit it; skip this step if already signed in
to Claude Code (or Codex). Neither Archon nor Aether needs its own login for these engines.
Start below from the root of a GitHub checkout where you can push a branch and open a PR.
The repository needs a commit, an `origin` remote, and your git name and email configured.

```sh
gh auth login
archon setup
archon aether models
```

Check that the configured orchestrator model appears in the model list;
[change it](docs/configuration.md#select-an-available-model) if needed before continuing.

```sh
archon repo add . --name example --ship pr --pipeline docs
archon doctor
archon
```

`archon` opens the terminal UI on the orchestrator. Type the request there, for example:

> In repository example, use the docs pipeline to add a short guide to the existing test commands. Verify each command and open a pull request. Do not merge.

The `docs` pipeline writes, independently verifies, and ships the change; the orchestrator
answers there and reports the PR URL. Reply to its questions in place; `/quit` leaves, and work continues.
`archon say "…"` sends one request and exits; `archon chat` is a line chat for SSH, scripts and pipes.

`archon` is the CLI; `archond` manages the queue and runs. Commands start
the daemons when needed. By default, Archon uses its own Aether instance,
seeded from your Aether configuration by `archon setup`.

## Core ideas

- **Item:** one queued task with a brief, priority, dependencies, and optional repository.
- **Pipeline:** a named sequence of stages, chosen for the kind of work.
- **Stage:** a work step, such as implementation, testing, review, or shipping.
- **Gate:** a rule that requires stage verdicts or operator approval before continuing.
- **Model plan:** the model and reasoning effort selected for each stage, checked against your model policy.
- **Autonomy:** five knobs for review, proposal, report, merge, and tool access, bounded by your ceiling.
- **Ship mode:** `pr` opens a PR, `local` keeps commits on the run branch.
- **Orchestrator:** the agent you chat with; it answers directly, reads your code in mirrors `archond` keeps for it, delegates wide reads and research to helpers, and queues every change as an item.

## Built-in pipelines

| Pipeline | When to use it |
| --- | --- |
| `adaptive` | Changes where one lead keeping its context saves restarts: it sizes the work, delegates useful parts, and has the integrated result reviewed once (`review = standard`; chosen by name or as a repository's `default_pipeline`) |
| `quick-pr` | Small changes that do not need an independent review stage |
| `default-pr-review` | Implementation with review and a fix loop |
| `strict-pr-review` | Higher-risk changes needing tests and three independent reviews |
| `team-pr` | Changes that split into backend and frontend roles |
| `bugfix` | Reproduce a bug, fix it, and check the regression |
| `docs` | Documentation with independent claim, command, and link verification |
| `dependency-upgrade` | Dependency updates with compatibility and audit checks |
| `refactor` | Change structure while preserving behavior |
| `performance` | Improve a measured slow path |
| `test-coverage` | Add tests for existing behavior |
| `agent-testing` | Have agents exercise the product as users |
| `e2e-testing` | Test complete user journeys and report the results |
| `code-audit` | Inspect code through three independent review lenses |
| `ask` | Answer a repository question without a PR |
| `errand` | A small task without repository changes or outward side effects |
| `design-doc` | Research and critique a design before implementation |

### Write your own pipeline

Create `my-pipeline/pipeline.toml` under `~/.config/archon/pipelines/` (Linux default; see [configuration](docs/configuration.md) for other locations) for all repositories, or under `<repo>/.archon/pipelines/` for one:

```toml
[pipeline]
name = "my-pipeline"
description = "Answer a repository question"
version = 1
workspace = "repo-readonly"
[[stage]]
id = "answer"
kind = "work"
prompt = "answer.md"
```

Beside it, create `answer.md` containing `Answer {{task.brief}} without changing files.`, then run `archon pipelines check` (add `--repo <repo>` for a repository pipeline); the [pipeline guide](docs/PIPELINES.md) covers stages, gates, fix loops and replacing built-ins by name.

## Everyday commands

Replace `<id>` with an item ID from the queue; a unique prefix also works.

| Command | Purpose |
| --- | --- |
| `archon` / `archon say "…"` | Open the terminal UI, or send one request and exit |
| `archon status` | Show health, capacity, spending, and waiting work |
| `archon queue list` / `archon queue show <id> --full` | Inspect items and stage results |
| `archon queue retry <id>` / `archon queue cancel <id>` | Retry a failed item or cancel work |
| `archon queue caps` | Inspect or change runtime limits |
| `archon pause` / `archon resume` | Hold or resume dispatch; add `--hard` for running agents |
| `archon questions` / `archon answer <number> "…"` | Read and answer operator questions |
| `archon log --item <id> --follow` | Follow one item's events |
| `archon reports` | Find report files and review pages |
| `archon cost --since 7d --by repo` | Show spending by repository |
| `archon model-evidence` | Inspect recorded stage costs, durations, and verdicts |
| `archon github-watcher add example-org/example-repo` | Route authorized `@archon` requests to the orchestrator |
| `archon aether schedule --help` | Use Aether's schedules for recurring requests |
| `archon pipelines list` / `archon pipelines check` | List or validate available pipelines |

See [scripted queue usage](docs/queue-scripting.md) for enqueueing from files
and waiting for results, or the [user guide](docs/guide.md) for chat and lifecycle details.

## Configuration

The file is `~/.config/archon/config.toml` on Linux; macOS uses
`~/Library/Application Support/archon/config.toml`. `ARCHON_CONFIG_DIR`
overrides the directory. Common settings are:

- `[orchestrator] model`: the agent you talk to.
- `[models] allow`, `deny`, `max_effort`: an optional narrowing. Without it Archon may use every model Aether reports ready, and the orchestrator picks one per stage.
- `[limits]`: concurrent agents, per-repository caps, budgets, and minimum free disk.
- `[autonomy]`: default knobs and their maximum permitted values.
- `[worktrees]`: location and cleanup timing for run worktrees and build caches.

[Configuration reference](docs/configuration.md). A repository's
`.archon/config.toml` can narrow limits and autonomy.

## Safety model

The daemon enforces the autonomy ceiling; agents cannot raise it through MCP. Worktrees are not sandboxes.
Merging is manual by default; automatic merging needs your permission in the ceiling and on the item.
The watcher accepts human writers, maintainers, or admins, further restricted by any allowlist; only explicitly configured operator logins can carry your authority.
Budgets hold or stop work based on recorded usage, which can arrive late; they are not prepaid spending guarantees.
Read [SECURITY.md](SECURITY.md) and the [GitHub watcher guide](docs/github-watcher.md) before widening access.

## Documentation

[User guide](docs/guide.md) · [Configuration](docs/configuration.md) ·
[Pipelines](docs/PIPELINES.md) · [Queue scripting](docs/queue-scripting.md) ·
[GitHub watcher](docs/github-watcher.md) · [Architecture](docs/ARCHITECTURE.md)

Project version: **1.0.1**. Public home: [mengsig/archon](https://github.com/mengsig/archon).
Licensed under [MIT](LICENSE).
For development, see [CONTRIBUTING.md](CONTRIBUTING.md).
