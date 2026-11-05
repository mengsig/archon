# Release 1.5.0 smoke gate

**Verdict: DECREASE, on a confirmed Luna cost increase.** The release head
`a3cc3920` on Aether 1.3.0 cost a mean of $0.0682 per Luna run over three
runs. Two fresh v1.4.12 runs on Aether 1.2.11 cost $0.0520: +31%, beyond
the ±10% noise band. The first two head runs already averaged $0.0619
(+19%), and the third run confirmed it at $0.0809. The increase is repairs:
the head had 4 unshipped runs in 3 runs ($0.0234 per run), v1.4.12 had none
that a review caused. The pass rate did not fall: 12/12 on the head against
7/8 on v1.4.12. Luna wall time was 604 s per run on the head against
829 s on v1.4.12, but v1.4.12's figure includes a trial that ran to its
721 s time limit. Without that trial v1.4.12 averages 468 s, and the head
is 29% slower, again from the repairs.

**Isolation.** One run of the head on Aether 1.2.11 cost $0.0492, passed
4/4 and had no repair: 5% below v1.4.12 and inside the range of its two
runs ($0.0443 and $0.0597). Archon 1.5.0 on Aether 1.2.11, the Aether that
1.4.12 shipped with, does not show the increase. The increase shows only with Aether 1.3.0, but
one isolation run and the spread of repairs across runs (0, 2 and 2 on the
head) cannot tell Aether 1.3.0 from run-to-run variance in review outcomes.

Haiku is within noise: $0.1166 per run on the head against ≥ $0.1225 on
v1.4.12 (−5%), 8/8 on both, with no repairs on the head.

Per the release rule, nothing is fixed here. The operator decides.

| Model | Arm | Runs | Trials passed | Archon cost/run | Wall/run | Repairs/run | Review cost/run |
|---|---|---|---|---|---|---|---|
| Haiku | v1.4.12, Aether 1.2.11 | 2 | 8/8 | ≥ $0.1225 | 346 s | 0.5 (1, 0) | $0.0233 |
| Haiku | head `a3cc3920`, Aether 1.3.0 | 2 | 8/8 | $0.1166 (−5%) | 363 s | 0 (0, 0) | $0.0235 |
| Luna | v1.4.12, Aether 1.2.11 | 2 | 7/8 | $0.0520 * | 829 s † | 0 (0, 0) ‡ | $0.0094 * |
| Luna | head `a3cc3920`, Aether 1.3.0 | 3 | 12/12 | $0.0682 * (+31%) | 604 s | 1.33 (0, 2, 2) | $0.0127 * |
| Luna | isolation: head `a3cc3920`, Aether 1.2.11 | 1 | 4/4 | $0.0492 * (−5%) | 506 s § | 0 | $0.0055 * |

- `*` Luna costs are Aether's estimates: Codex usage is unbilled. Haiku
  costs are billed. `≥` marks a run in which Aether reported a trial's
  cost as incomplete (an interrupted agent).
- `†` v1.4.12 Luna run 1 includes a `quality-retry-budget` trial that ran
  to its 721 s time limit. Without that trial the arm averages 468 s.
- `‡` That trial's one unshipped run ($0.0101) was cancelled by the trial
  timeout, not by a review. It is not counted as a repair.
- Review cost is summed from each run's Aether data over every agent with
  role `reviewer`.
- `§` The isolation run started at a host load of 16.0 (see Method).

## What drives the Luna increase

- **Repairs, not the shipped work.** The four unshipped head runs cost
  $0.0701 in all. Without them the head costs $0.0448 per run. That is
  close to v1.4.12's $0.0470 per run without its timeout-cancelled run. One
  head run with no repairs cost $0.0464, between the two v1.4.12 runs.
- **Head run 2, `quality-ledger-transfer` at `strict`.** The gate refused
  two attempts. In the first, one of its four reviewers returned a summary
  whose JSON had no `verdict`. In the second, the lead named 1 reviewer
  where `strict` needs 3. The orchestrator then queued a follow-up item at
  `standard`, which shipped. The task cost $0.0428 and took 365 s, against
  $0.0127 to $0.0198 on v1.4.12. These are the same refused-result shapes
  1.4.11's first gates recorded.
- **Head run 3, two genuine review fails.** On `quality-ledger-transfer` a
  reviewer found that unhashable account arguments raised `TypeError`
  instead of the specified `KeyError`. On `quality-retry-budget` a
  reviewer found a wrong saturation threshold (`base_ms=5`, `cap_ms=6`
  returns 6, not 5). Both reviewers returned `request_changes` with a
  blocking finding. Both leads still reported a pass that listed the
  finding. The gate refused it ("its verdict passes but lists a blocking
  finding"), and the repairs shipped. Those two tasks cost $0.0310 and
  $0.0372.
- **Review level.** Head run 2 reviewed two tasks at `strict`. Every
  v1.4.12 Luna run reviewed every task at `standard`.
- **What changed on the stage path.** Between `v1.4.12` and `a3cc3920`,
  `crates/archon-pipeline` (the stage contract) is unchanged, and so is
  `crates/archon-daemon/src/executor` (the review gate) apart from one
  visibility change in `frozen.rs`. The changes on an agent's path are the
  supervisor's stall recovery (#393, `supervisor/stall.rs`) and Aether 1.3.0
  itself. The isolation run (head on Aether
  1.2.11, the same Archon code) had no repair and cost $0.0492, so the
  Archon change alone does not reproduce the increase.
- **Against the 1.4.12 gate's record.** 1.4.12's gate recorded $0.0597 per
  Luna run on its release head, and the operator accepted it. The 1.5.0
  head's $0.0682 is 14% above that.

## Pass rate

- **Luna.** The one miss is v1.4.12 run 1's `quality-retry-budget`. Its
  stage lead delegated a reviewer, then called `wait_for` on its own task
  id instead of the reviewer's. The reviewer finished at 21:06:58, and the
  lead sat waiting until the trial's time limit cancelled it at 21:16:44.
  The head had no miss.
- **Haiku.** No miss on either arm.
- **Aether arm.** Every run on both arms passed 4/4. The Aether arm runs no
  Archon code.

## Runs

In the order they ran, one at a time:

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Repairs (cost) | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|---|
| Haiku v1.4.12, run 1 | 1.2.11 | 4/4 | $0.1402 | 376 s | 4 ($0.0310) | 1 ($0.0261) | 4/4 | PASS |
| Haiku head, run 1 | 1.3.0 | 4/4 | $0.1166 | 380 s | 3 ($0.0244) | 0 | 4/4 | PASS |
| Haiku v1.4.12, run 2 | 1.2.11 | 4/4 | ≥ $0.1048 | 315 s | 2 ($0.0156) | 0 | 4/4 | PASS |
| Haiku head, run 2 | 1.3.0 | 4/4 | $0.1165 | 345 s | 3 ($0.0225) | 0 | 4/4 | PASS |
| Luna v1.4.12, run 1 | 1.2.11 | 3/4 | $0.0443 * | 1041 s | 3 ($0.0075) | 0 ‡ | 4/4 | FAIL (pass rate) |
| Luna head, run 1 | 1.3.0 | 4/4 | $0.0464 * | 400 s | 2 ($0.0033) | 0 | 4/4 | PASS |
| Luna v1.4.12, run 2 | 1.2.11 | 4/4 | $0.0597 * | 616 s | 5 ($0.0113) | 0 | 4/4 | PASS |
| Luna head, run 2 | 1.3.0 | 4/4 | $0.0773 * | 641 s | 9 ($0.0161) | 2 ($0.0279) | 4/4 | PASS |
| Luna head, run 3 (confirmation) | 1.3.0 | 4/4 | $0.0809 * | 771 s | 7 ($0.0188) | 2 ($0.0422) | 4/4 | PASS |
| Luna isolation (head on Aether 1.2.11) | 1.2.11 | 4/4 | $0.0492 * | 506 s § | 3 ($0.0055) | 0 | 4/4 | PASS |

- Haiku v1.4.12 run 1's repair was a refused review: the reviewer read a
  commit the run's workspace did not hold. It shipped on the repair.
- A repair is an Archon run of an item that did not ship. Its cost is
  already in the Archon cost.

Archon's cost and wall time per task, with the review level the
orchestrator chose (`standard` unless marked):

| Task | Luna old 1 | Luna old 2 | Luna head 1 | Luna head 2 | Luna head 3 |
|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0136 · 135 s | $0.0205 · 205 s | $0.0112 · 80 s none | $0.0098 · 85 s none | $0.0098 · 60 s none |
| `quality-ledger-transfer` | $0.0127 · 155 s | $0.0198 · 210 s | $0.0165 · 150 s | $0.0428 · 365 s strict, 2 repairs | $0.0310 · 325 s, 1 repair |
| `quality-retry-budget` | $0.0141 · 721 s **timeout** | $0.0157 · 170 s | $0.0153 · 140 s | $0.0202 · 165 s strict | $0.0372 · 360 s, 1 repair |
| `review-seeded-bug` | $0.0039 · 30 s | $0.0036 · 30 s | $0.0034 · 30 s | $0.0045 · 25 s | $0.0030 · 25 s |

| Task | Haiku old 1 | Haiku old 2 | Haiku head 1 | Haiku head 2 |
|---|---|---|---|---|
| `fix-off-by-one` | $0.0359 · 75 s | $0.0168 · 45 s none | $0.0261 · 70 s | $0.0267 · 70 s |
| `quality-ledger-transfer` | $0.0562 · 150 s, 1 repair | $0.0393 · 125 s | $0.0424 · 130 s | $0.0389 · 125 s |
| `quality-retry-budget` | $0.0360 · 110 s | ≥ $0.0363 · 105 s | $0.0385 · 115 s | $0.0396 · 120 s |
| `review-seeded-bug` | $0.0121 · 40 s | $0.0123 · 40 s | $0.0096 · 65 s | $0.0113 · 30 s |

The isolation run (head on Aether 1.2.11) cost $0.0137 · 150 s,
$0.0168 · 170 s, $0.0157 · 155 s and $0.0030 · 30 s on the same four tasks,
all at `standard`.

## Aether

- **Head arm: Aether 1.3.0.** Built from mengsig/aether commit
  `e42ac6423ad961f43890b74c461bc879280d2623` (the head of aether#414, with
  the archon#393 turn bound) in a read-only temp clone, with
  `cargo build --release --locked -p aether`. `aether version` reports
  `aether 1.3.0 (protocol 26)`. It serves protocols 17 to 26, so Archon's
  `PROTOCOL_VERSION` (24) and `MIN_PROTOCOL_VERSION` (17) connect
  unchanged. The clone held no change after the build.
- **Old arm: Aether 1.2.11.** The published `x86_64-unknown-linux-musl`
  archive, downloaded with `gh release download v1.2.11`. It passed the
  release's `SHA256SUMS` and its own `.sha256`. The `SHA256SUMS` file is
  byte-identical to the one the 1.4.12 gate recorded, and the binary
  reports `aether 1.2.11`.

## Method

The method is 1.4.12's, with the same `gate.sh` (byte-identical) and the
stored baselines `baseline.json` (Haiku) and `baseline-luna.json` (Luna,
`codex/gpt-6-luna`). `bench/` is unchanged between `v1.4.12` and
`a3cc3920`, apart from the 1.4.12 record itself.

- One trial per task and arm (`archon`, `aether`), medium effort, run
  serially, under the default $7 cap.
- Haiku first, then Luna, alternating v1.4.12 and head runs, one at a
  time, each inside the host CI slot (`sh scripts/ci-slot.sh`).
- **Load.** The host's 1-minute load was below 10 before every run but
  the isolation run. That run's load check passed before its first
  (refused) try, and it started at a load of 16.0 from other work on the
  host. `§` marks its wall time, which the load may have raised.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. It refused the isolation run four times (47
  to 18 minutes left) until the host refreshed the token. The run started
  at 23:15 UTC, an hour after the last head run.
- **Head arm.** `archon` and `archon-bench` were built from a `git archive`
  of `a3cc3920` with `cargo build --release -p archon -p archon-bench
  --locked`, on Aether 1.3.0.
- **Old arm.** The released `archon` 1.4.12 (`x86_64-unknown-linux-musl`
  archive, downloaded with `gh release download v1.4.12`, passing its
  `SHA256SUMS` and `.sha256`, reporting `archon 1.4.12`), with
  `archon-bench` built from a `git archive` of the `v1.4.12` tag, on the
  released Aether 1.2.11 that 1.4.12 shipped with.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held copies (mode 600) of the engine credentials (Claude
    and Codex) and the `archon-bench` git identity.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.
- **Logs.** The Aether logs hold no errors. Their warnings are the usual
  start-up ones (no systemd scope under `env -i`, auto-approval, temp dir
  length, no `socat`) and, on Haiku, notes that a closing receipt omitted
  streamed usage that the cumulative receipt covers.

## `make ci`

`make ci` ran on `a3cc3920` (tree `105a59ce`) from this item's worktree,
outside `/tmp`, with the operator's `ARCHON_*` and `AETHER_*` variables
unset. It exited 0 after 431 s: format, shell checks, the local release
orchestration tests, clippy with `-D warnings`, rustdoc with `-D warnings`,
and 106 test binaries with 4059 passed, 0 failed and 10 ignored. No test
needed a rerun.

## Evidence

This item's artifact (`release-1.5.0-gate-evidence`) keeps the results and
logs of every run, each run's Aether data directory, the gate and analysis
scripts, the build log, the `SHA256SUMS` files, and the `make ci` log.

| Binary | sha256 prefix |
|---|---|
| head `archon` (`a3cc3920`, release profile) | `0e1c9ab5` |
| head `archon-bench` (`a3cc3920`, release profile) | `d43e2c1f` |
| Aether 1.3.0 (`e42ac642`, release profile) | `23e856ff` |
| released Aether 1.2.11 (old arm) | `f8bc883d` |
| released v1.4.12 `archon` (old arm) | `7dcc7f33` |
| `archon-bench` from the `v1.4.12` tag (old arm) | `ef734b47` |
