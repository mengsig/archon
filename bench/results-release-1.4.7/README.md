# Release 1.4.7 smoke gate

**Verdict: PASS, no decrease.** On the release head `148eec71` (1.4.7, with
the four pre-release fixes), Archon passed 8 of 8 Haiku trials and v1.4.6
passed 8 of 8. Luna passed 4/4 on both builds. Both arms ran on the released
Aether 1.2.5.

`CARGO_BUILD_JOBS=4 make ci` passed on the same head: 3363 tests passed,
0 failed and 9 ignored.

**Cost: no increase.**

- On the head, Haiku's Archon arm cost a mean of $0.1221 per run, against
  $0.1247 on v1.4.6.
- In run 2 both builds chose `strict` review on `quality-ledger-transfer`
  (head $0.0662, v1.4.6 $0.0562). Every other Haiku run chose `standard`
  there.
- Leaving out `quality-ledger-transfer`, the Haiku means were $0.0749 on the
  head and $0.0674 on v1.4.6. Most of that gap is one trial,
  `quality-retry-budget` in head run 2 ($0.0386, against $0.0281-0.0317 in
  the other three). With one trial per cell, and the head's overall Haiku
  mean lower than v1.4.6's, this is not a cost increase, so it was not
  rerun.
- Luna's estimate was $0.0679 on the head against $0.0602 on v1.4.6. On
  v1.4.6, the orchestrator chose review `none` for `quality-retry-budget`,
  and `standard` on the head. Luna's estimates in the 1.4.6 gate ranged from
  $0.0501 to $0.0764 for one build.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 4/4 | $0.1037 | 291 s | 4/4 | PASS · $0.14 |
| Haiku v1.4.6, run 1 | 4/4 | $0.1293 | 396 s | 4/4 | PASS · $0.16 |
| Haiku head, run 2 | 4/4 | ≥ $0.1405 | 446 s | 4/4 | PASS · ≥ $0.18 |
| Haiku v1.4.6, run 2 | 4/4 | $0.1201 | 356 s | 4/4 | PASS · $0.16 |
| Luna head | 4/4 | $0.0679 * | 422 s | 4/4 | PASS · $0.08 * |
| Luna v1.4.6 | 4/4 | $0.0602 * | 371 s | 4/4 | PASS · $0.07 * |

- **Cost marks.** Haiku's costs are billed. Luna's (`*`) are Aether's
  estimates for a subscription engine, which bills $0, so they are not
  bounds. `≥` marks a cost that Aether reported as incomplete: head run 2's
  `quality-ledger-transfer` trial.
- **Gate cost.** These six runs cost ≥ $0.64 billed and $0.15 estimated.

## Method

The method is the 1.4.6 gate's:

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baselines `baseline.json` (Haiku) and `baseline-luna.json`
  (Luna).

The baselines, tasks and graders under `bench/` are byte-identical in v1.4.6
and on the head. The runs alternated between the head and v1.4.6, one at a
time.

- **Head arm.** `archon` and `archon-bench` were built from `148eec71` with
  `cargo build -p archon -p archon-bench --locked` (dev profile).
- **v1.4.6 arm.** The released `archon` 1.4.6 (`x86_64-unknown-linux-musl`
  archive, from `gh release download v1.4.6`). The archive passed both the
  release's `SHA256SUMS` and its own `.sha256` check. `archon-bench` was
  built from the `v1.4.6` tag.
- **Aether, both arms.** The released Aether 1.2.5
  (`x86_64-unknown-linux-musl` archive), which passed both its release's
  `SHA256SUMS` and its own `.sha256` check. It reports `aether 1.2.5`.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held copies (mode 600) of the engine credentials and the
    `archon-bench` git identity.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.

## Findings

- **Daemon logs.** The archond logs of both builds show only the usual
  tightening of the sandbox's socket directory and, on runs whose agents'
  usage was revised down, "usage total went down; keeping what was already
  charged" (head: 2 Haiku and 3 Luna lines; v1.4.6: 2 Luna lines). That
  warning dates from 1.0.0. No other warning or error appeared.
- **Aether protocol floor.** Archon 1.4.7 raises the lowest Aether protocol
  it accepts to 17. The released Aether 1.2.5 serves 17 to 24. Every head
  run connected to it at protocol 24.
- **What reaches the agents.** Between v1.4.6 and the head:
  - the built-in prompts and the stage contract diff against
    `{{base_commit}}`, the fork point, instead of `{{base_branch}}`;
  - the orchestrator's prompt says the TUI's read commands are answered for
    it, and that it can schedule recurring work;
  - the orchestrator and stages no longer see Aether's native `notify`, and
    stages no longer see `refresh_provider_usage`.
  None of the smoke tasks ran a refresh or a gate answer.

## Evidence

This item's artifact (`release-1.4.7-gate-evidence`) keeps:

- the results and logs of the six runs;
- the sandbox scripts (`scripts/gate.sh`, `scripts/run.sh`);
- the `make ci` log;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`148eec71`) | `4e0a3205` |
| head `archon-bench` (`148eec71`) | `7236e403` |
| released v1.4.6 `archon` | `acdc7128` |
| `archon-bench` v1.4.6 tag | `2f9555b9` |
| released Aether 1.2.5 | `0c40c972` |
