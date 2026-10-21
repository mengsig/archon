# Troubleshooting

## An initiative is stuck

Inspect it with `archon initiatives list` and `archon initiatives show <id>`. These need a running daemon.

- **A member failed or was cancelled.** The initiative is `failed`. `archon initiatives retry <id>` re-queues the failed members and keeps the ones that succeeded.
- **The initiative should not ship.** `archon initiatives cancel <id>` cancels its unfinished members.
- **The initiative is `done` but a member runs again** (retried, refreshed or recovered after a restart). The initiative stays `done`. Its pull requests return to draft on the next maintenance pass while any member is not `succeeded`, and Archon logs a warning naming the initiative. They are marked ready again once every member has succeeded.
- **Maintenance of one initiative keeps failing.** Archon tells you once with a blocker notification and retries on every pass; other initiatives and items are not held back. The error text names what to fix, for example a member's repository or branch.

## The daemon stops soon after every start

Read `archond`'s log (the `archond stopped error=...` line names the failing task). A panic in the coordination pass is logged and retried on the next pass, so a single bad record no longer stops the daemon. If an older version panics at start (for example `assertion failed: !ready || passed` in `initiative_ship.rs`, issue #200), update Archon: the bad record needs no manual repair, and no offline tool to edit the store exists.
