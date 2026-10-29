# Troubleshooting

## Where archond's log is

A detached `archond` writes `archond.log` next to its socket: `$XDG_RUNTIME_DIR/archon/archond.log`, or `archond.log` in the data directory when there is no runtime directory. `archon daemon start` prints the exact path as `startup log: …`, and repeats it when the daemon exits before it listens. The file rotates, keeping the active file and three archives (`archond.log.1` …). `archon log` is something else: it shows the notifications and outcomes archond posted, not its own log lines.

## An initiative is stuck

Inspect it with `archon initiatives list` and `archon initiatives show <id>`. These need a running daemon.

- **A member failed or was cancelled.** The initiative is `failed`. `archon initiatives retry <id>` re-queues the failed members and keeps the ones that succeeded.
- **The initiative should not ship.** `archon initiatives cancel <id>` cancels its unfinished members.
- **The initiative is `done` but a member runs again** (retried, refreshed or recovered after a restart). Queueing the member again reopens the initiative (`open`), so it never sits `done` with a member that is not `succeeded`. Its pull requests return to draft while any member is not `succeeded`; when every member has succeeded again the initiative is `done` again and the orchestrator is told once more. A member of a `cancelled` initiative cannot be retried alone: run `archon initiatives retry <id>`.
- **Maintenance of one initiative keeps failing.** Archon tells you once with a blocker notification and retries on every pass; other initiatives and items are not held back. The error text names what to fix, for example a member's repository or branch.

## The daemon stops soon after every start

Read `archond`'s log (the `archond stopped error=...` line names the failing task). A panic in the coordination pass is logged and retried on the next pass, so a single bad record no longer stops the daemon. If an older version panics at start (for example `assertion failed: !ready || passed` in `initiative_ship.rs`, issue #200), update Archon: the bad record needs no manual repair, and no offline tool to edit the store exists.

## A tool call answered `pending: req_…`

The daemon did not answer the change within the MCP call's 15 s budget, but may still carry it out, so do not repeat it blindly. Call `request_status` with that `req_…` id: it answers `applied` with the change's result, `failed` with its error, `pending` while it still runs, or `interrupted` if archond stopped during it. A change made with an `idempotency_key` can also be repeated with the same key and arguments: it is never applied twice, and the repeat answers with the first result or status. `request_status` only shows the caller's own requests, and finished ones are kept for 24 h.

## A notification says a daemon task failed

- **"restarted every time. Everything else keeps running."** One of archond's background tasks failed several times in a row and archond keeps restarting it. Run `archon doctor` and report its output if the notice comes back.
- **"switched off. Everything else keeps running."** The task failed too often and archond stopped it. `archon daemon restart` starts it again; if it fails again, report the output of `archon doctor`.
- **"A panic archond isolated leaves it running"** archond recorded a crash or panic in an earlier run and posts it once at the next start. The notice names the place and time. If archond is stopped, `archon daemon start` starts it, and the log lines before the crash are in `archond.log`.
- **"it is the config file of Archon before 1.0"** An old `archon.toml` sits in your config directory and is not read. Run `archon setup` to write the current commented config file, copy over the settings you still want (docs/configuration.md lists the keys), then delete the old file.

## Dispatch is held

- **"Dispatch is held:"** from the disk check. Free space is below `limits.min_free_gb`, or a write failed on a full disk. No new agent starts until space is back. Run `archon gc` to reclaim space.
- **"is above [limits] max_load_per_cpu"** (not a blocker; shown by `archon status` and `archon queue show <id>`). The host's load per CPU is over your limit, so nothing new starts and running agents finish. It resumes by itself once load drops; raise `max_load_per_cpu` under `[limits]` in your config (docs/configuration.md) to hold less.
- **"is below [limits] min_available_memory_gb"** (not a blocker). The host has less free memory than your limit, so nothing new starts. It resumes when memory is free again; lower `min_available_memory_gb` under `[limits]` to hold less.
- **"agents at once, but the caps now allow"** A team stage needs more agents at once than the caps permit, so it waits. Raise the cap named in the notice: `archon queue caps --max-parallel-agents <N>`, or set `max_per_repo` / `max_per_pipeline` in the config, or run `archon queue cancel <id>`.
- **"Blocked:"** An item was blocked because a dependency failed or was cancelled; the text ends with `dependency <id>`. Run `archon queue show <id>` on the item and on the dependency, then `archon queue retry <dependency>` or `archon queue cancel <id>`.

## A budget is reached

- **"No new work starts; running agents finish."** The daily or total budget is reached. Raise it: `archon queue caps --budget-daily <USD>` or `archon queue caps --budget-total <USD>`; the daily budget also resets at local midnight. `archon status` shows spend and caps.
- **"Daily budget reached"** / **"Total budget reached"** (not blockers) is what a waiting item shows in `archon queue show <id>` for the same hold; the next step it names is the matching `archon queue caps` command.
- **"the item's budget is used up"** One item spent its own budget and failed with `BLOCKED:`. Raise it with `archon queue steer <id> --budget-usd <USD>`, then `archon queue retry <id>`.
- **"cannot enforce the item's dollar budget"** Aether reported usage without a dollar cost, so the item's budget cannot be checked. Configure model pricing in Aether or pick a model that reports cost, then `archon queue retry <id>`.
- **"and a budget:"** The item asks for a review that its budget forbids. Run `archon queue steer <id> --budget-usd <USD>` or change the item's review, then `archon queue retry <id>`.

## An item failed or cannot start

- **"Full detail: archon queue show"** Every failed item is posted as `Failed: <title> — <stage>: <cause>`. Run `archon queue show <id> --full` for the whole story; `archon queue retry <id>` queues it again once fixed.
- **"queues it again once fixed"** The item failed before any run existed (`could not start: …`). Fix what the text names, then run `archon queue retry <id>`.
- **"cannot resume the run:"** After a restart archond could not rebuild this run, for example because its pipeline no longer loads. Fix the cause, then `archon queue retry <id>`, or give it up with `archon queue cancel <id>`.
- **"create_agent failed on a full disk"** Aether's disk is full; Archon's had room. Free space where Aether runs, then `archon queue retry <id>`.
- **"refused at spawn"** The stage's model is outside your model policy. Change `[models]` in the config or steer the stage to an allowed model (`archon queue steer <id> --stage <stage> --model <PROVIDER/MODEL>`), then `archon queue retry <id>`.
- **"so the run does not go on beside them"** Aether refused to stop the agents a stage had delegated to. Run `archon aether agents` to see them, then `archon queue retry <id>`.
- **"still run in Aether after their cancel"** A stage ended, but agents it delegated to are still running. The item waits for them; `archon aether agents` shows them.
- **"merging the team's branches failed"** The branches of a team stage could not be merged. Run `archon queue show <id> --full` for the git error, fix the branch, then `archon queue retry <id>`.
- **"the lead is still fixing it"** A review has failed many rounds. The run goes on; stop it with `archon queue cancel <id>` or change course with `archon queue steer <id> --note "…"`.
- **"tries again on every maintenance pass"** Maintenance of one item or initiative keeps failing. The error text names what to fix, for example a repository or branch; Archon says nothing more until it passes.

## Shipping and GitHub

- **"has no `origin` remote to push to"** The repository has no `origin`. Add one, or set the repository's ship mode to `local` (`archon repo set <repo> --ship local`), then `archon queue retry <id>`.
- **"refused the push of"** `origin` rejected the push. Its branch rules or your access decide this; fix them, then `archon queue retry <id>`.
- **"Point `origin` back at"** `origin` no longer points at the repository the pull request's branch lives in, so nothing was pushed. Point it back, or queue the work again without `onto`.
- **"has no token for"** `gh` has no token for the GitHub login the repository runs as. Run `gh auth login --hostname github.com` as that user, or change it with `archon repo set <repo> --gh-user <LOGIN>`.
- **"was refused for want of access"** GitHub refused an action for lack of access. Fix the token or permission it names, then `archon queue retry <id>`.
- **"Auto-merge could not be enabled for"** The pull request is delivered but will not merge by itself. Merge it on GitHub, or run `archon merge <id>`.
- **"Not merged:"** A merge you or the orchestrator asked for was refused; the text after the colon says why. Fix that, then `archon merge <id>` again (`--ignore-checks` if the checks are the reason and you accept the risk).
- **"This is not a failure of the change; fix the GitHub Actions account or runners"** The pull request's checks never started. Fix GitHub Actions for the repository, then re-run the checks on GitHub.
- **"`origin` rejected the push as not a fast-forward"** Restacking a pull request found commits on GitHub that Archon does not have. Archon never force-pushes: merge `origin/<branch>` into the branch by hand, or push it yourself.
- **"Nothing was written, and Archon never rebases or"** A stacked item's parent merged, and merging the base into the item's branch conflicts in the files named. Run `archon queue refresh <id>`: an agent merges the base, resolves the conflict and updates the same pull request.
- **"over its merged parent merged"** Restacking an item over its merged parent found commits on its pull request that Archon does not have, so the push was refused. Archon never force-pushes: merge `origin/<branch>` into the branch by hand, then `archon queue refresh <id>`.
- **"is red at"** An integration branch fails its full check; a fix item is queued (`Queued fix item <id>`). Follow it with `archon queue show <id>`.
- **"merging it waits for the operator"** The integration branch is green and a pull request is open into the release branch. Run `archon promote <repo>`.

## The orchestrator or Aether is down

- **"Archon's own aetherd is not running, and archond could not start it"** archond retries every few minutes. `archon daemon start` tries at once and says what fails; `archon doctor` checks the setup.
- **"Archon revives it again in"** The orchestrator is stopped or failing, and the next automatic revival is that many seconds away; "Archon is reviving it now" means it is already due. `archon orchestrator` shows its state; `archon orchestrator restart` starts a fresh one now.
- **"Archon keeps reviving it, waiting longer each time"** The orchestrator stopped many times in a row. Its news is held until it recovers; item outcomes still appear. When the notice says its engine `is not logged in`, run the command it names (for example `claude` once), or set `[orchestrator] model` to a provider that is logged in; the next revival picks it up. Otherwise read `archond.log`, then run `archon orchestrator restart`.
- **"minutes without a message, tool result or usage update"** The orchestrator's turn looks hung. Run `archon orchestrator restart`; ignore the notice if you know it is waiting on a long command.
- **"Archon will not start the orchestrator:"** `[orchestrator] model` is outside your model policy. Change `[orchestrator] model` or `[models]` in the config, then `archon orchestrator restart`.
- **"Archon could not bring back the archon MCP server in Aether"** The orchestrator has none of Archon's tools. `archon doctor` shows the server's state; heal it by hand with `archon aether mcp refresh archon`, then `archon orchestrator restart`.
- **"could not restart the orchestrator to give it Archon's tools back"** The MCP server is back but the orchestrator did not restart. Run `archon orchestrator restart`.
- **"Aether is not naming the agents that call Archon's tools"** Aether does not send caller names, so archond cannot tell the orchestrator from agents it delegated to. The notice gives the fix for your mode; restarting needs `archon daemon stop --aether`, then `archon daemon start`.
- **"record of replaced orchestrators was unreadable"** Archon reset its record of old orchestrators. Agents they delegated to may still run and their spend is not counted: check `archon aether agents`.
- **"Cancelled agent"** An agent was cancelled because it broke the model or depth policy, and the reason follows. Review `[models]` in the config if you did not expect it.

## An engine is not logged in

- **"is on PATH, so agents cannot run yet"** Neither `claude` nor `codex` is installed. Install one, log in by running it once, then run `archon` again.
- **"is not logged in: run"** (`archon doctor`'s `engine` check, `archon setup`, and the orchestrator's revival blocker) The orchestrator's engine has no login. Run the command it names, for example `claude` once, or set `[orchestrator] model` to the logged-in engine it suggests. Restarting the orchestrator does not help until then.
- **"aetherd runs a stale archon server"** (an `archon doctor` warning) Aether still serves the tools of an older Archon. Run the one command it names: `archon aether mcp refresh archon` for Archon's own Aether, `aether mcp refresh archon` with `--shared`.
- **"`archon aether login <provider>` if Archon needs it"** (an `archon doctor` warning) Archon's own Aether does not share your Aether credentials. Run `archon aether login <provider>` for the provider you use.
- **"Waiting for provider quota: every provider stage"** (not a blocker) Every provider the stage may use is out of quota or not usable. It starts once one has quota again. Name another provider or model with `archon queue steer <id> --model <PROVIDER/MODEL>`.

## Processes left running after items finish

- **"compose stacks left by finished items"** (an `archon doctor` warning) A Docker Compose project still has containers, running or stopped, in the worktree of an item that ended or was pruned. archond tears these down when a run ends and before gc removes the worktree, then removes the volumes they made if the project is named after the item (other names may be shared with a checkout elsewhere, so their volumes stay: `docker volume ls --filter label=com.docker.compose.project=<project>` lists them), so this is a stack from before, or one whose teardown failed (the feed says why: **"Could not tear down the Docker Compose stack"**). Run the `docker compose --project-name <project> down --remove-orphans` it names; add `--volumes` once you no longer need the stack's data. archond leaves alone any project that also has a container outside the worktree, and does nothing when `[worktrees] docker` is not installed or its daemon does not answer.
- **"beyond the … reports archond follows"** (an `archon doctor` warning) More `lavish-axi poll` processes run than archond has open reports: polls of a page archond no longer follows, or a second poll of one page. Run the `kill <pids>` it names. archond follows a report until its session ends, its item ends, a newer page of the same title replaces it, or 24 hours after it was shown, even while a gate waits on it (answer that gate in the chat, or `archon reports reopen` the page), and kills its polls when it stops.

## `archon tui` times out attaching

**"try `archon tui` again after Aether reconnects"** comes with archond's reason, such as `archond did not answer` … `within`: archond is up but its link to Aether or the orchestrator stayed unavailable for 5 seconds. Run `archon status` and `archon doctor`, wait for Aether to reconnect, then run `archon tui` again.

A failed orchestrator is not a timeout: `archon tui` opens the TUI on it, so its error is on screen, and `archon chat` exits with 1. `archon doctor` names the fix; when it is not a login, `archon orchestrator restart` starts a fresh one.

## `archon update` seems to hang

archond waits for running stage agents to finish before it restarts, up to 30 minutes by default.

- **"gave up the restart: stage agents still ran after"** Nothing was restarted. Ask again with `archon update --wait <SECS>`, or `archon update --requeue` to restart anyway; stopped stages are retried.
- **"see its startup log with"** No new archond answered after the restart. Read `archond.log` (see the first section) and run `archon daemon status`.
