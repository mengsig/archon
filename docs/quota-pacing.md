# Quota pace

Archon reads each provider's quota windows from Aether (at most every 30
seconds, beside dispatch). An unweighted dispatch pass never waits for this
read; a weighted one that finds it in flight takes its answer, or its
failure, so it waits no longer than its own read would. Archon then
compares, for every window, how much of the quota is spent with how much of
the window's time has passed:

- used `u = 100 − remaining_percent`;
- elapsed `e = 100 × (1 − time to reset / window length)`;
- at the rate so far, the window runs out `remaining × elapsed time / used`
  after the reading.

A window is **short** when `u > e + [models] pacing_margin_percent` (default
10). At that rate it runs out before it resets.

## What Archon does

Archon throttles nothing. It tells the operator, in the feed, and the
orchestrator, as a `quota_pace` trigger, once per provider window when that
window is short: the provider and window, the used and elapsed percent, the
projected empty time and the reset time. The orchestrator then chooses what
to run: fewer items at once, lower efforts, or another provider. The same
window is not told again. The next one is told again if it also runs short.

The trigger also carries every provider's window pace at that moment, so
the orchestrator can choose between providers.

`archon status` and `queue_status` show each window's pace after its
remaining percent, for example
`pace 7d used 78% at 29% elapsed, empty ~2026-11-21 04:00 UTC, before its reset 2026-11-24 12:00 UTC`
for a short window, or `5h used 20% at 60% elapsed, empty ~… , resets …`
for one on pace (`no rate yet` while nothing is spent).

## When there is no pace

A window has no pace, and is never warned of, when:

- Aether's report is stale, its usage observation is over 10 minutes old,
  or Archon's reading is;
- the window does not give its length or its absolute reset time (Aether
  1.4.1 may leave either out; a countdown alone is not enough);
- the window has already reset.

## Configuration

```toml
[models]
pacing_margin_percent = 10   # 0 to 100; global only
```
