# Archon benchmark, v3 (Archon 1.1.0, cost work)

- Runs `20261014T091017Z` (round 1) and `20261014T095439Z` (round 2): 2026-10-14T09:10:17Z → 2026-10-14T10:18:19Z
- Archon trees: round 1 is `1298fb0` ("Have the orchestrator queue independent changes in one message"), round 2
  is `0f61cb7` ("Build initiative members at once and report their outcomes together"), both on branch
  `speed-cost`. The raw `run.json` files name `a3dc5ef` and `27ad3ed`, the same trees before a rebase.
- Arms: `archon` and `aether`, as in results-v2
- Effort: `high`; parallel: 2 (archon trials one at a time, as the harness requires)
- Model, both arms: `claude-code/claude-opus-5-5`. The archon orchestrator chose `claude-code/claude-opus-5-5` for
  every item, at effort `low` for 21 of the 23 items and `medium` for the other 2 (round 1, `multi-repo-currency`).
- Versions: `archon 1.1.0`, `aetherd 1.0.3`
- Trials: each round ran every task once per arm (16 trials; $12.99 and $12.26). 32 trials in total, 32
  completed, 0 skipped, 0 infrastructure failures; spent **$25.25**.

## Per arm

| Arm | Model | Trials | Success | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-opus-5-5` | 16 | 14/16 (88%) | 0.92 ± 0.25 | 156 ± 158 | 797844 ± 507743 | 13981 ± 10276 | 1.19 ± 0.75 | 2.7 ± 1.5 | 0 | 0 |
| aether | `claude-code/claude-opus-5-5` | 16 | 15/16 (94%) | 0.94 ± 0.25 | 77 ± 43 | 246575 ± 161269 | 7297 ± 4744 | 0.39 ± 0.17 | 2.9 ± 2.2 | 0 | 0 |

Every task has two trials per arm, so the per-arm means weigh each task equally. pass@2 is 100% for both arms: no
task failed twice.

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 2/2 · 33 ± 4 · 0.43 ± 0.00 · 1.0 ± 0.0 | 2/2 · 33 ± 6 · 0.22 ± 0.02 · 1.0 ± 0.0 |
| feature-token-bucket | 2/2 · 510 ± 191 · 1.83 ± 0.56 · 2.0 ± 0.0 | 2/2 · 89 ± 36 · 0.43 ± 0.12 · 3.0 ± 1.4 |
| fix-off-by-one | 2/2 · 38 ± 4 · 0.42 ± 0.00 · 2.0 ± 0.0 | 2/2 · 24 ± 6 · 0.17 ± 0.00 · 1.0 ± 0.0 |
| long-backlog | 1/2 · 158 ± 32 · 2.29 ± 0.07 · 5.0 ± 0.0 | 2/2 · 154 ± 8 · 0.71 ± 0.05 · 5.5 ± 2.1 |
| multi-repo-currency | 1/2 · 146 ± 6 · 1.71 ± 0.06 · 4.5 ± 0.7 | 2/2 · 100 ± 9 · 0.49 ± 0.01 · 5.5 ± 2.1 |
| multi-repo-pagination | 2/2 · 186 ± 41 · 1.62 ± 0.01 · 4.0 ± 0.0 | 2/2 · 101 ± 24 · 0.46 ± 0.01 · 5.0 ± 1.4 |
| refactor-order-status | 2/2 · 126 ± 6 · 0.93 ± 0.01 · 2.0 ± 0.0 | 1/2 · 67 ± 16 · 0.36 ± 0.03 · 1.5 ± 0.7 |
| review-seeded-bug | 2/2 · 50 ± 0 · 0.31 ± 0.01 · 1.0 ± 0.0 | 2/2 · 49 ± 1 · 0.25 ± 0.01 · 1.0 ± 0.0 |

## Archon's overhead against the aether arm, per task

Ratios of the per-task means, archon ÷ aether. "Orchestrator $" is the trial's cost minus what its queue items
spent (`archon-items.json`), i.e. what the orchestrator agent itself cost. Every item ran `direct`, as one agent
that delegates its own reviewers at the item's review level.

| Task | Wall × | Input tok × | Cost × | Orchestrator $ | Items | Pipelines the orchestrator chose (round 1; round 2) |
|---|---:|---:|---:|---:|---:|---|
| ask-cache-invalidation | 0.98 | 1.72 | 1.98 | 0.43 | 0, 0 | answered itself |
| feature-token-bucket | 5.73 | 5.05 | 4.26 | 0.30 | 1, 1 | direct at standard; review failed twice, then once, and each fix was reviewed |
| fix-off-by-one | 1.55 | 3.33 | 2.52 | 0.24 | 1, 1 | direct with no review |
| long-backlog | 1.03 | 2.71 | 3.24 | 0.45 | 4, 4 | 4 × direct, 2 at standard and 2 with none; 4 × direct at standard |
| multi-repo-currency | 1.46 | 3.57 | 3.47 | 0.39 | 2, 3 | 2 × direct at standard; the same and a follow-up for a regression a reviewer noted |
| multi-repo-pagination | 1.84 | 3.23 | 3.49 | 0.37 | 2, 2 | 2 × direct at standard |
| refactor-order-status | 1.88 | 3.66 | 2.57 | 0.32 | 1, 1 | direct at standard |
| review-seeded-bug | 1.03 | 2.01 | 1.24 | 0.31 | 0, 0 | answered itself |
| **All 8 (sum of task means)** | **2.02** | **3.24** | **3.09** | 29% of archon $ | | |

## Compared with results-v2

All eight tasks, each weighed once (sum of per-task means). v2 had one or two trials per task; v3 has two.

| Metric | v2 archon | v3 archon | change | v2 aether | v3 aether | change | Overhead v2 | Overhead v3 | v3 ÷ v2 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Pass rate | 13/13 | 14/16 | −1 task-trial | 13/13 | 15/16 | −1 task-trial | — | — | — |
| Wall s | 1131 | 1246 | +10% | 571 | 617 | +8% | 1.98× | 2.02× | 1.02 |
| Input tokens | 7.92 M | 6.38 M | −19% | 1.88 M | 1.97 M | +5% | 4.20× | 3.24× | 0.77 |
| Cost $ | 11.16 | 9.54 | −15% | 3.02 | 3.09 | +2% | 3.69× | 3.09× | 0.84 |

Without `feature-token-bucket`, whose archon wall time is review rounds (below), archon's wall sum falls 5%
(778 → 736 s) and its wall overhead goes from 1.56× to 1.39×.

Per task, archon (v2 → v3) / aether (v2 → v3), and the overhead (v2 → v3):

| Task | Wall s | Input k tok | Cost $ | Wall × | Cost × |
|---|---|---|---|---:|---:|
| ask-cache-invalidation | 30 → 33 / 35 → 33 | 119 → 184 / 93 → 107 | 0.39 → 0.43 / 0.20 → 0.22 | 0.86 → 0.98 | 1.91 → 1.98 |
| feature-token-bucket | 353 → 510 / 73 → 89 | 1413 → 1244 / 194 → 246 | 1.78 → 1.83 / 0.37 → 0.43 | 4.86 → 5.73 | 4.76 → 4.26 |
| fix-off-by-one | 50 → 38 / 19 → 24 | 452 → 338 / 90 → 102 | 0.51 → 0.42 / 0.16 → 0.17 | 2.63 → 1.55 | 3.22 → 2.52 |
| long-backlog | 210 → 158 / 143 → 154 | 2753 → 1575 / 496 → 582 | 3.76 → 2.29 / 0.66 → 0.71 | 1.47 → 1.03 | 5.73 → 3.24 |
| multi-repo-currency | 135 → 146 / 98 → 100 | 1059 → 1098 / 347 → 307 | 1.63 → 1.71 / 0.52 → 0.49 | 1.38 → 1.46 | 3.17 → 3.47 |
| multi-repo-pagination | 145 → 186 / 103 → 101 | 1018 → 1047 / 379 → 324 | 1.56 → 1.62 / 0.52 → 0.46 | 1.41 → 1.84 | 2.99 → 3.49 |
| refactor-order-status | 163 → 126 / 61 → 67 | 866 → 632 / 193 → 173 | 1.22 → 0.93 / 0.38 → 0.36 | 2.69 → 1.88 | 3.24 → 2.57 |
| review-seeded-bug | 45 → 50 / 40 → 49 | 242 → 264 / 92 → 131 | 0.30 → 0.31 / 0.22 → 0.25 | 1.13 → 1.03 | 1.37 → 1.24 |

## Failures

- **Round 1, `long-backlog`, archon.** The four tickets were built in parallel and each passed its own tests, but
  the fixture has no `.gitignore` and the implementers committed `__pycache__/*.pyc` files. Those binary caches
  conflicted when the grader merged the four branches, so TT-102's branch was left out (`note` in
  `results.jsonl`) and the hidden CSV tests ran against the old exporter. Round 2's tree tells work stages not to
  commit build caches (`da94cb3`); its four branches merged cleanly and it passed.
- **Round 2, `multi-repo-currency`, archon.** The `ledger` implementer read "when it is missing the currency is
  `USD`" as covering an empty fourth column; the hidden test expects `""` to be malformed (exit 2). The
  orchestrator's brief listed the malformed cases without the empty one, and the item's reviewer passed it. The
  aether arm read it the other way in both rounds.
- **Round 2, `refactor-order-status`, aether.** It left status string literals in a test in `src/report.rs`,
  which the grader's "no status literals outside `src/status.rs`" check refuses.

## Verdict

The operator's reading: these results are good, and cost is not the priority now. Against v2, Archon's cost
overhead fell from 3.69× to 3.09× and its input-token overhead from 4.20× to 3.24×, while the aether arm stayed
flat (+2% cost). Wall-time overhead is unchanged (1.98× → 2.02×), and all of the increase is in
`feature-token-bucket`. There the reviewer found real defects the hidden tests do not check (a `wait_time`
loop that hangs on a large or backwards clock), and each fix went back to review. Round 2, with one re-review,
took 343 s and $1.14 against round 1's 611 s and $1.91.

A single small task still pays a fixed price: the orchestrator's own turns, $0.24–0.49 a trial (v2: $0.30–0.76),
and 29% of Archon's spend. That price is spread out on longer work. `long-backlog` (four tickets) is now at wall
parity (1.03×; round 2 135 s for Archon against 160 s for Aether), and its cost overhead fell from 5.73× to
3.24×. On a backlog of many long tasks, the fixed orchestrator cost becomes a small part of the bill.

Quality was close. Archon lost one trial to committed build caches, which round 2 no longer shows, and one to an
ambiguous spec. Aether lost one trial to a refactor that was not finished.

## Caveats

- n is 2 per cell. `feature-token-bucket` alone spreads 343–611 s for Archon depending on how many review rounds
  it ran. Treat the ratios as a direction.
- The two rounds ran different trees: round 2 adds the build-cache rule and a failed review answered through a
  review of its fix (`5a655ab`). Both rounds predate the pipeline consolidation (`363d8da`); every item ran
  `direct` anyway, which the consolidation keeps.
- The arms ran on a private sandbox aetherd (1.0.3, its own config under `~/.local/share/archon-bench/lf`), not
  the operator's daemon, as in v2.
- Round 2's trials are renumbered `trial = 2` in the merged `results.jsonl`; the raw per-round files are
  untouched.
- Wall time excludes Archon's daemon start-up and the harness's quiet window, as in v1 and v2.

## Harness changes

None. Both rounds used the release `archon-bench` binary with `--archon-bin` and `--aether-socket`; the task
`pipeline` hints now say `direct` (they were `default-pr-review`), which the orchestrator reads only as a hint.

## Reproduce

```sh
cargo build --release -p archon -p archon-bench
archon-bench run --arms archon,aether --trials 1 --parallel 2 --effort high \
  --model claude-code/claude-opus-5-5 --archon-bin <archon 1.1.0> --aether-socket <aetherd.sock> --max-usd 30
```

Run it twice; merge the two `results.jsonl` files with the second's `trial` set to 2 and render with
`archon-bench report <dir>`.

## Files

- `results.jsonl`, `run.json`, `chart.json`, `report.html`, `harness-summary.md`: both rounds merged, rendered
  with `archon-bench report`.
- `raw/round1-20261014T091017Z/`, `raw/round2-20261014T095439Z/`: each run's own output as the harness wrote it.

The method is the harness's own, as in results-v1 and results-v2: the same prompt, effort, time limit and model
for both arms; permission prompts bypassed; fresh fixture copies outside the checkout; hidden graders applied
after every agent process stopped; costs as billed by the provider through Aether.
