# Troubleshooting

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
