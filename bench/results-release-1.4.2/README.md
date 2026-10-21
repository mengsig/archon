# Release 1.4.2 smoke gate

**Verdict: PASS, no decrease.** On the release head `5a310ff8` (1.4.2) Haiku
passed 8/8 Archon trials in two runs, the same as v1.4.1 (8/8), and Luna
passed 4/4 on both. Every `archon-bench smoke` run reported PASS against the
stored baselines. `CARGO_BUILD_JOBS=3 make ci` passed on the same head (3140
tests passed, 0 failed, 9 ignored).

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 4/4 | $0.0961 | 907 s | 4/4 | PASS · $0.13 |
| Haiku v1.4.1, run 1 | 4/4 | $0.1265 | 361 s | 4/4 | PASS · $0.16 |
| Haiku head, run 2 | 4/4 | $0.0921 | 307 s | 4/4 | PASS · $0.13 |
| Haiku v1.4.1, run 2 | 4/4 | $0.0947 | 401 s | 4/4 | PASS · $0.13 |
| Luna head | 4/4 | $0.0447 * | 271 s | 4/4 | PASS · $0.06 * |
| Luna v1.4.1 | 4/4 | $0.0463 * | 280 s | 4/4 | PASS · $0.06 * |

Haiku's costs are billed. Luna's (`*`) are Aether's estimates for a
subscription engine (billed $0), so they are not bounds.

## Method

One trial per task and arm (`archon`, `aether`), medium effort, serial, the
default $7 cap, the stored baselines `baseline.json` (Haiku) and
`baseline-luna.json` (Luna), which are byte-identical in v1.4.1 and on the
head, as are the tasks and graders. Runs were interleaved (head, v1.4.1,
head, ...), one at a time.

- **Head arm:** `archon` and `archon-bench` built (`cargo build -p archon -p
  archon-bench --locked`, dev profile) from `5a310ff8`, with Aether built
  from mengsig/aether `origin/next` at `2296a61`, which holds every 1.2.2
  change (its version bump is a separate pull request, so it still reports
  1.2.1).
- **v1.4.1 arm:** the released `archon` 1.4.1 and Aether 1.2.1
  (`x86_64-unknown-linux-musl` archives, each checked against its release's
  `SHA256SUMS`), under `archon-bench` built from the `v1.4.1` tag, as the
  1.4.1 gate ran each build under its own harness.
- **Sandbox:** each run had a new private directory under
  `~/.local/share/archon-bench/` with its own aetherd, Archon and Aether
  config and data, sockets and `TMPDIR`, started through an `env -i`
  wrapper. Unlike the 1.4.1 gate, `HOME` was private too: it held copies
  (mode 600) of the engine credentials and a git identity of its own
  (`archon-bench`). The aetherd was stopped by its socket and PID and the
  whole sandbox, credentials included, removed after each run. No operator
  daemon, configuration or data was used.

## Findings

- **No pass-rate change.** Archon 12/12 on the head against 12/12 on v1.4.1
  over both models; the `aether` arm passed 12/12 on both.
- **Cost** of the Haiku Archon arm was lower or equal on the head ($0.0961
  and $0.0921 against $0.1265 and $0.0947). Luna's estimates match within 4%.
- **One slow trial.** In Haiku head run 1, Archon's `fix-off-by-one` took
  676 s (its `direct` implement stage ran from 20:44:35 to 20:55:26), against
  65-75 s in every other run of that task on either build. It passed, well
  inside the 20-minute limit, and the same task took 65 s on the head in
  run 2. That run kept no agent transcripts, so the cause is not known; the
  later runs keep each sandbox's Aether data in the evidence. v1.4.1 had a
  slow trial of its own (`review-seeded-bug`, 161 s against 20-40 s, in run
  2). Without that trial head run 1's Archon wall is 231 s.
- **What reaches the agents.** Between v1.4.1 and the head the orchestrator
  prompt (`crates/archon-daemon/prompts/orchestrator.md`, #192, #194), the stage
  contract (`crates/archon-pipeline/src/contract.rs` and
  `contract/reviewers.rs`, #191, #207) and the swarm build prompt (#195)
  changed. The smoke results show no effect of these on the four tasks.

### A discarded first attempt

The first head run, in a sandbox whose private `HOME` had no git identity,
scored Archon 1/4: each implement agent made and tested its fix but could not
commit ("Author identity unknown") and stopped with `BLOCKED:`. That is the
sandbox's fault, not Archon's (the 1.4.1 gate ran with the operator's `HOME`,
which has an identity), and the run is not counted. Every counted run had the
`archon-bench` identity in its `HOME`. Its results are kept with the evidence
(`results-invalid/`).

## Evidence

Results, logs, the scripts that made each sandbox (`gate.sh`, `gate2.sh`),
the `make ci` log and the sha256 of every binary are kept as this item's
artifact (`release-1.4.2-gate-evidence`). Head `archon` sha256 `57b8826d`,
released v1.4.1 `archon` `6cded021`, Aether next `ace3d26c`, released Aether
1.2.1 `220a88dd`.
