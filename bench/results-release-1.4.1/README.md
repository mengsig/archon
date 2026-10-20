# Release 1.4.1 smoke gate

**Verdict: PASS WITH ONE UNREPRODUCED MISS.** At `70aebe96` (1.4.1) the
first Haiku run of next passed 3/4 Archon trials against a 4/4 baseline, and
`archon-bench smoke` reported FAIL. Its repeat passed 4/4. v1.4.0 passed 4/4
on Haiku and Luna alike. Luna on next passed 4/4. No change between v1.4.0
and 1.4.1 touches the orchestrator prompts. A reader holding the rule "if
performance decreased, report it and stop" should treat the Haiku miss as a
reported, unreproduced regression signal. `make ci` is green (2978 tests, 0
failed).

## Method

One trial per task and arm (`archon`, `aether`), medium effort, serial, a $7
cap, each run in its own new private sandbox under `archon-bench/` with an
`env -i` environment, and the stored baselines `baseline.json` (Haiku) and
`baseline-luna.json`. Each sandbox ran its own aetherd, built from
mengsig/aether `96cfaad0` (1.2.1), and was stopped after its run. Next was
`archon` 1.4.1 at `70aebe96`; the baseline arm was `archon` built from
`v1.4.0`. No operator daemon was used.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku next, run 1 | 3/4 | $0.0929 | 300 s | 4/4 | FAIL |
| Haiku next, run 2 | 4/4 | $0.1253 | 391 s | 4/4 | PASS |
| Haiku v1.4.0 | 4/4 | $0.0977 | 375 s | 4/4 | PASS |
| Luna next | 4/4 | see below | 459 s | 4/4 | PASS |
| Luna v1.4.0 | 4/4 | see below | 521 s | 3/4 | PASS |

Luna's Archon cost is an Aether estimate (`cost_gap` mixed); the smoke totals
for the whole run, both arms, were $0.07 for next and $0.08 for v1.4.0.

## Findings

- **The Haiku miss** is `review-seeded-bug` in run 1. The orchestrator
  answered that it could not review `src/tokens.js` because no `session-lib`
  repository was registered, and declined to read the one repository there,
  `review-seeded-bug`. It queued no item (15 s, $0.0058). The same task
  passed in run 2 and on v1.4.0. That is the model's refusal, not an Archon
  failure to start or deliver work; one sample cannot show whether 1.4.1 makes
  it likelier.
- **Cost.** Haiku run 1 cost less than v1.4.0 ($0.0929 against $0.0977), though
  its review trial did no work. Run 2 cost +28% ($0.1253), almost all from one trial
  (`quality-ledger-transfer`, $0.06 against $0.03). One trial per cell cannot
  separate that from noise.
- **Luna** passes all four tasks on next, at a lower wall time and smoke total
  than v1.4.0.
- The v1.4.0 Luna `aether` arm missed `review-seeded-bug`, the lexical miss
  the 1.4.0 gate described; it does not gate.

Run results are kept as this item's artifact (`release-1.4.1-gate-evidence`).

## Rerun: is the Haiku miss a flake?

**Verdict: flake.** Three more Haiku smoke runs of this branch passed 4/4
Archon trials each, as did three more of v1.4.0. Across all runs the miss
count is 1/20 Archon trials on next (5 runs) against 0/16 on v1.4.0 (4 runs).
The `review-seeded-bug` failure did not recur in 6 further trials, so there
is no transcript to read for it; a single refusal in 20 trials does not
separate 1.4.1 from v1.4.0.

Same method as above: the same Aether 1.2.1 (`96cfaad0`) and the same
`archon` binaries, a new private `env -i` sandbox and aetherd per run, one run
at a time, medium effort, the Haiku baseline, no operator daemon. Runs were
interleaved (next, v1.4.0, next, ...).

| Run | Archon | Aether | Smoke total |
|---|---|---|---|
| Haiku next, rerun 1 | 4/4 | 4/4 | PASS · $0.14 |
| Haiku v1.4.0, rerun 1 | 4/4 | 4/4 | PASS · $0.14 |
| Haiku next, rerun 2 | 4/4 | 4/4 | PASS · $0.13 |
| Haiku v1.4.0, rerun 2 | 4/4 | 4/4 | PASS · $0.17 |
| Haiku next, rerun 3 | 4/4 | 4/4 | PASS · ≥ $0.16 |
| Haiku v1.4.0, rerun 3 | 4/4 | 4/4 | PASS · $0.13 |

`review-seeded-bug` on Archon took 30-50 s and cost about $0.01 in all six
runs. Between v1.4.0 and this branch no file with prompt, orchestrator,
repository or mirror in its path changed (`git diff --name-only v1.4.0..HEAD`),
which agrees with the earlier finding that the orchestrator prompts are
unchanged. The rerun artifacts were not kept; the sandboxes were removed.
