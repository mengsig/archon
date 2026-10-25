# Release 1.4.6 smoke gate

**Verdict: PASS, no decrease.** On the release head `725ee998` (1.4.6),
Archon passed 12 of 12 trials: two gate runs and one confirming Haiku run.
v1.4.5 passed 8 of 8. Luna passed 4/4 on both builds.

`CARGO_BUILD_JOBS=4 make ci` passed on the same head: 3308 tests passed,
0 failed and 9 ignored.

**Cost: higher on the head, but from the orchestrator's choice of review level
on one task, not from a change in this release.**

- On the head, Haiku's Archon arm cost a mean of $0.1276 per run over the two
  gate runs, against $0.0988 on v1.4.5.
- On `quality-ledger-transfer` the orchestrator chose `strict` review in both
  head gate runs ($0.0522 and $0.0797). The same task costs about $0.03 under
  `standard`.
- The confirming head run chose `standard` and cost $0.0916, level with
  v1.4.5's $0.0910.
- Over all six Haiku runs on head commits, the head cost a mean of $0.1147
  against $0.1031 on v1.4.5. Leaving out that one task, the head cost less:
  $0.0631 against $0.0674.
- Luna's estimate was lower on the head ($0.0639 against $0.0764).

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 4/4 | ≥ $0.1131 | 356 s | 4/4 | PASS · ≥ $0.16 |
| Haiku v1.4.5, run 1 | 4/4 | $0.1065 | 320 s | 4/4 | PASS · $0.14 |
| Haiku head, run 2 | 4/4 | $0.1420 | 387 s | 4/4 | PASS · $0.18 |
| Haiku v1.4.5, run 2 | 4/4 | $0.0910 | 280 s | 4/4 | PASS · ≥ $0.13 |
| Luna head | 4/4 | $0.0639 * | 356 s | 3/4 | PASS · $0.08 * |
| Luna v1.4.5 | 4/4 | $0.0764 * | 430 s | 3/4 | PASS · $0.09 * |
| Haiku head, run 3 (confirmation) | 4/4 | $0.0916 | 281 s | 4/4 | PASS · $0.13 |

- **Cost marks.** Haiku's costs are billed. Luna's (`*`) are Aether's
  estimates for a subscription engine, which bills $0, so they are not
  bounds. `≥` marks a cost that Aether reported as incomplete.
- **Gate cost.** These seven runs cost $0.73 billed and $0.17 estimated.
- **First round.** A first round of seven runs used `35b3b1ae`, the release
  commit before the last pre-release fix (`c5b3a15d`). It cost $0.74 billed
  and $0.13 estimated. Its results are under "First round" below.

## Method

The method is the 1.4.5 gate's:

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baselines `baseline.json` (Haiku) and `baseline-luna.json`
  (Luna).

The baselines, tasks and graders under `bench/` are byte-identical in v1.4.5
and on the head. The runs alternated between the head and v1.4.5, one at a
time.

- **Head arm.**
  - `archon` and `archon-bench` were built from `725ee998` with
    `cargo build -p archon -p archon-bench --locked` (dev profile).
  - Aether was built (dev profile) from `ba46e719`, the head of the Aether
    1.2.5 release pull request (mengsig/aether#300). The binary reports
    `aether 1.2.5`.
  - That pull request has since merged into Aether's `next` as `9842dbb1`,
    with the same tree.
- **v1.4.5 arm.**
  - The released `archon` 1.4.5 and Aether 1.2.4
    (`x86_64-unknown-linux-musl` archives).
  - Each archive passed both its release's `SHA256SUMS` and its own `.sha256`
    check.
  - `archon-bench` was built from the `v1.4.5` tag.
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

- **Review level on `quality-ledger-transfer`.** Neither build changed the
  review guidance.
  - The orchestrator chose `strict` in 3 of 6 head Haiku runs over both
    rounds, and in none of 4 on v1.4.5.
  - v1.4.5's first Luna run chose `strict` on the same task too, and so did
    v1.4.4's second run in the 1.4.5 gate.
  - Orchestrators on both builds read `SPEC.md` before enqueueing in some
    runs. A brief that lists the spec's concurrency and overflow cases tends
    to get `strict`.
  - Under `strict` the head trials passed and cost $0.0522-0.0850, against
    $0.0288-0.0503 under `standard` on either build.
  - Every other task cost the same or less on the head, on average.
- **New daemon behaviour.** The head's archond logs show no stall verdict, no
  reap and no new warning, so the watchdog and the reaper stayed quiet on
  healthy runs. The only warning lines were the usual tightening of the
  sandbox's socket directory, and "usage total went down; keeping what was
  already charged". That one dates from 1.0.0 and, across both rounds,
  appeared in more trials on v1.4.5 (9) than on the head (3).
- **What reaches the agents.** Between v1.4.5 and the head, the pipelines and
  the stage contract did not change. The orchestrator's prompt changed in
  three ways:
  - two lines about `request_status`;
  - one settings-topic line on `checks = "waived"`;
  - the tool list:
    - it gains `request_status`;
    - the keyed change tools gain an optional `idempotency_key`;
    - `refresh_item` gains `model_plan`, `model_reasons` and
      `stage_timeouts`.
- **`make ci` under an Archon stage.** The first `make ci`, on `35b3b1ae`,
  failed six `archon` CLI tests in `dedicated`, `isolation`, `json_output`
  and `shutdown`.
  - **Cause.** The stage's shell inherits `ARCHON_DAEMON_DETACHED=1` and the
    `ARCHON_*` locations from the operator's archond. The test archonds then
    took themselves for detached daemons and opened their log in a missing
    run directory.
  - **Not a regression.** That code is unchanged since v1.4.5.
  - **Workaround.** With those four variables removed, the same tests pass.
    Every `make ci` reported here ran that way.

## First round

This round ran on `35b3b1ae`, before the fix `c5b3a15d`. That fix bounds the
tracked outcomes archond keeps in memory and records them later.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 3/4 | $0.1005 | 387 s | 4/4 | FAIL · $0.14 |
| Haiku v1.4.5, run 1 | 4/4 | $0.1209 | 370 s | 4/4 | PASS · $0.16 |
| Haiku head, run 2 | 4/4 | $0.1494 | 662 s | 4/4 | PASS · $0.19 |
| Haiku v1.4.5, run 2 | 4/4 | $0.0940 | 316 s | 4/4 | PASS · $0.13 |
| Luna head | 4/4 | $0.0529 * | 322 s | 4/4 | PASS · $0.07 * |
| Luna v1.4.5 | 4/4 | $0.0501 * | 257 s | 3/4 | PASS · $0.06 * |
| Haiku head, run 3 (confirmation) | 4/4 | $0.0918 | 291 s | 4/4 | PASS · $0.13 |

- **The head's miss: Haiku run 1, `review-seeded-bug`.** It was the model's,
  and the mirror was in place.
  - The prompt calls the repository `session-lib`, but the bench registers
    it as `review-seeded-bug`.
  - The orchestrator ran `list_repos` and then `ls repos/`, and both named
    `review-seeded-bug`.
  - It then looked for a `session-lib` mirror and searched the disk against
    its instructions.
  - Finally it asked the operator which file to review.
  - Every other `review-seeded-bug` Archon answer in both rounds and in the
    1.4.5 gate reviewed the mirror.
- **Head run 2.** It chose `strict` review on `quality-ledger-transfer`
  ($0.0850, 451 s).
- **Round total.** The head passed 11 of 12 trials in this round, and both
  rounds together 23 of 24. v1.4.5 passed 16 of 16.

## Evidence

This item's artifact (`release-1.4.6-gate-evidence`) keeps:

- the results and logs of both rounds;
- the sandbox scripts (`scripts/gate.sh`, `scripts/run.sh`);
- the `make ci` logs;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`725ee998`) | `ff766b0e` |
| first-round `archon` (`35b3b1ae`) | `e2abfb18` |
| released v1.4.5 `archon` | `6918d4fa` |
| Aether 1.2.5 PR head | `bb0f0ff2` |
| released Aether 1.2.4 | `3be708e3` |
