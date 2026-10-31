# Release 1.4.9 smoke gate

**Verdict: PASS with an accepted cost increase. The operator accepted the
Haiku review-cost rise (x2.2, from #329's reviewer effort floor) as
intended on 2026-10-31; pass rate is unchanged (4/4 in the last three head
runs).** The release head is `6fbc077a` (1.4.9). v1.4.8 runs on Aether 1.2.7.

- **History.** The gate first recorded FAIL: by the release rule a
  confirmed cost increase stops the release until the operator decides.
  The numbers below are that gate's, unchanged.

- **Haiku cost.** On the published Aether 1.2.8 the head cost $0.2314 and
  $0.2347 per Archon run, against $0.0989 and $0.1163 for v1.4.8. That is
  2.2 times v1.4.8's mean of $0.1076. Every one of the four head Haiku
  runs cost more than either v1.4.8 run.
- **Haiku pass rate.** The head passed 15 of 16 trials, v1.4.8 8 of 8.
  - The miss is head run 1's `quality-ledger-transfer`. It used `strict`
    review and hit the 1200 s trial limit while its ninth reviewer was
    still reading. The code it left grades as a pass, so this run's smoke
    was first recorded as FAIL.
  - Head run 2 (the confirmation) and both runs on the published Aether
    passed 4/4. The pass-rate drop did not reproduce.
- **Luna.** Both builds passed 4/4. The head's estimate is lower: $0.0440
  against $0.0682.
- **Stop.** Head runs 3 and 4 confirmed the cost increase. By the release
  rule the gate stopped there for the operator's decision, so Luna was not
  rerun on the published Aether 1.2.8.

**Cause: the reviewer effort floor from #329.** It is a deliberate change,
but its cost is the gate's to report.

- In 1.4.8 a reviewer with no model plan ran at effort `low`. 1.4.9 asks for
  at least `medium` (and `high` when the lead rates the risk medium or
  high; these leads rated it high but still delegated at `medium`).
- Every v1.4.8 reviewer in this gate ran at `low`, and every head reviewer
  at `medium`.
- A `medium` Haiku reviewer reads longer: 13 s to 5.5 min, against 12 to
  52 s at `low` in the 1.4.8 gate. It also fails more changes, mostly for
  test quality, such as a concurrency test that a mutant still passes.
- `strict` review asks for three reviewers. After a fix, all three are
  replaced by new whole-change reads (#329).
- Each of the three strict head trials delegated nine `medium` reviewers.
  The 1.4.8 gate's strict trials delegated three to six `low` ones, at
  $0.054 to $0.086 a trial. The head's strict trials cost $0.158, $0.165
  and ≥ $0.293.
- Standard review costs more too. `quality-ledger-transfer` cost $0.0751
  on the head under `standard`, against $0.0344 and $0.0479 on v1.4.8.
- **Decision.** The gate asked the operator to either accept the higher
  review cost as the price of #329 or bring the reviewer floor or the
  strict re-review back down before 1.4.9. On 2026-10-31 the operator
  accepted the cost as intended and released 1.4.9. A 1.4.10 follow-up is
  to make strict re-reviews after a fix reread only what changed.

`make ci` passed on `6fbc077a`, with the inherited Archon and Aether
variables unset: 3671 tests passed, 0 failed and 9 ignored. `make e2e`
passed 30 of 30 browser tests on the same head. The commit after it adds
only this file.

| Run | Aether | Archon | Archon cost | Archon wall | Aether arm | Smoke |
|---|---|---|---|---|---|---|
| Haiku v1.4.8, run 1 | 1.2.7 | 4/4 | $0.0989 | 306 s | 4/4 | PASS · $0.14 |
| Haiku head, run 1 | 1.2.8 built | 3/4 | $0.3669 | 1552 s | 4/4 | FAIL (timeout) · $0.41 |
| Haiku v1.4.8, run 2 | 1.2.7 | 4/4 | $0.1163 | 403 s | 4/4 | PASS · $0.15 |
| Haiku head, run 2 (confirmation) | 1.2.8 built | 4/4 | ≥ $0.1447 | 471 s | 4/4 | PASS · ≥ $0.18 |
| Luna v1.4.8 | 1.2.7 | 4/4 | $0.0682 * | 1616 s | 4/4 | PASS · $0.08 * |
| Luna head | 1.2.8 built | 4/4 | $0.0440 * | 876 s | 4/4 | PASS · $0.06 * |
| Haiku head, run 3 | 1.2.8 published | 4/4 | $0.2314 | 643 s | 4/4 | PASS · $0.27 |
| Haiku head, run 4 (confirmation) | 1.2.8 published | 4/4 | $0.2347 | 637 s | 4/4 | PASS · $0.27 |

- **Cost marks.** Haiku's costs are billed. Luna's (`*`) are Aether's
  estimates for a subscription engine, which bills $0, so they are not
  bounds. `≥` marks a cost that Aether reported as incomplete: head run
  2's `quality-ledger-transfer` trial (at least $0.0751).
- **Gate cost.** These eight runs cost ≥ $1.42 billed and $0.13 estimated.
- **Aether arm.** Aether alone passed 4/4 in every run, at $0.036 to
  $0.040 per Haiku run on both releases, so the increase is Archon's.

Archon's cost per task (Haiku, billed):

| Task | v1.4.8 run 1 | v1.4.8 run 2 | head 1 | head 2 | head 3 | head 4 |
|---|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0235 | $0.0249 | $0.0248 | $0.0249 | $0.0258 | $0.0240 |
| `quality-ledger-transfer` | $0.0344 | $0.0479 | $0.2927 strict, timeout | ≥ $0.0751 | $0.1580 strict | $0.1645 strict |
| `quality-retry-budget` | $0.0299 | $0.0328 | $0.0394 | $0.0346 | $0.0356 | $0.0358 |
| `review-seeded-bug` | $0.0112 | $0.0107 | $0.0100 | $0.0102 | $0.0120 | $0.0104 |

Reviewers delegated per Haiku run, from each run's Aether data: v1.4.8
2 and 3, all `low`; head 11, 5, 11 and 11, all `medium`.

## Aether 1.2.8

The head arm first ran on Aether built from `mengsig/aether` branch
`release-1.2.8` at `80033664`, in a scratch clone (`cargo build --release
--locked -p aether`), because v1.2.8 was not yet published. That is head
runs 1 and 2 and the Luna head run. v1.2.8 was then published from
`8ce219e7`, whose tree (`e239865a`) is the tree of `80033664`. Head runs 3
and 4 used the published `x86_64-unknown-linux-musl` archive. It passed both
the release's `SHA256SUMS` and its own `.sha256` check, and reports
`aether 1.2.8`. Both builds serve protocols 17 to 25, and every head trial
connected at protocol 24.

## Method

The method is the 1.4.8 gate's, with the same scripts (in the evidence,
`scripts/gate.sh` and `scripts/run.sh`):

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baselines `baseline.json` (Haiku) and `baseline-luna.json`
  (Luna).

`bench/` is byte-identical in v1.4.8 and on the head. The runs went one at
a time, in the table's order.

- **Head arm.**
  - `archon` and `archon-bench` were built from a `git archive` of
    `6fbc077a` with `cargo build --release -p archon -p archon-bench
    --locked`.
  - Aether is 1.2.8, as above.
- **v1.4.8 arm.**
  - The released `archon` 1.4.8 (`x86_64-unknown-linux-musl` archive, from
    `gh release download v1.4.8`). The archive passed both the release's
    `SHA256SUMS` and its own `.sha256` check.
  - `archon-bench` was built from the `v1.4.8` tag.
  - Aether is the released Aether 1.2.7, which 1.4.8 was released with. It
    passed the same two checks, matches the 1.4.8 gate's recorded hash, and
    reports `aether 1.2.7`.
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
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. No run was refused.

## Findings

- **Aether protocol.** Aether 1.2.8 serves protocol 17 to 25. Archon asks
  for 24, so every head trial connected at 24, and every v1.4.8 trial to
  aetherd 1.2.7 at 24.
- **Head run 1's timeout.** The `strict` lead had three reviewer rounds of
  three:
  - the first round had one fail for test quality: the snapshot-atomicity
    test passed against a mutant;
  - the second had one fail, again for test quality, on the test of the
    rule that opposite transfers must not deadlock;
  - the third had two passes in, and the third reviewer was still reading
    when the trial limit hit at 1202 s.
- **Logs.** The Aether logs show the usual warnings on both releases:
  - at start-up, yolo approvals, no systemd scope, the engine's network
    limits and the agent temp directory's long path;
  - Claude closing receipts that omitted streamed usage;
  - a runtime gone before its reaped processes were recorded.

## Evidence

This item's artifact (`release-1.4.9-gate-evidence`) keeps:

- the results and logs of all eight runs, with each run's Aether data
  directory;
- the sandbox scripts and `reviewers.py`, which counts each stage's
  reviewers with their effort and verdict;
- the `make ci` and `make e2e` logs;
- the `SHA256SUMS` of the three releases;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`6fbc077a`, release profile) | `375f98b2` |
| head `archon-bench` (`6fbc077a`, release profile) | `99cf413c` |
| released v1.4.8 `archon` | `d4f12cbd` |
| `archon-bench` v1.4.8 tag | `c5cbbfdb` |
| Aether 1.2.8 built from `80033664` | `78f5ceeb` |
| released Aether 1.2.8 | `72a80507` |
| released Aether 1.2.7 | `04888830` |
