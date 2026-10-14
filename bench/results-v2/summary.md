# Archon benchmark, v2 (Archon 1.1.0)

- Runs `20261014T071033Z` (round 1) and `20261014T073720Z` (round 2): 2026-10-14T07:10:33Z → 2026-10-14T07:54:53Z
- Archon commit: `9b5ddbc8e0eb37925582a6b765c3717269361a67` (branch `adaptive-orchestrator`, release 1.1.0)
- Arms: `archon` and `aether` only (`claude` and `codex` were not rerun; see results-v1 for them)
- Effort: `high`; parallel: 2 (archon trials one at a time, as the harness requires)
- Model, both arms: `claude-code/claude-opus-5-5`, the model results-v1 used. The archon orchestrator chose
  `claude-code/claude-opus-5-5` at effort `medium` for every stage it planned.
- Versions: `archon 1.1.0`, `aetherd 1.0.3`, Claude Code `2.1.296`
- Trials: round 1 ran every task once per arm (16 trials, $13.44). Round 2 repeated the five tasks where the arms
  differed most in round 1 (`feature-token-bucket`, `long-backlog`, `fix-off-by-one`, `multi-repo-pagination`,
  `refactor-order-status`; 10 trials, $11.67). 26 trials in total, 26 completed, 0 skipped, 0 infrastructure
  failures; spent **$25.11**.

## Per arm

| Arm | Model | Trials | Success | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-opus-5-5` | 13 | 13/13 (100%) | 1.00 ± 0.00 | 158 ± 119 | 1109611 ± 855682 | 18751 ± 15648 | 1.54 ± 1.16 | 3.1 ± 1.7 | 0 | 0 |
| aether | `claude-code/claude-opus-5-5` | 13 | 13/13 (100%) | 1.00 ± 0.00 | 74 ± 43 | 248929 ± 161467 | 7492 ± 4789 | 0.39 ± 0.18 | 2.8 ± 2.2 | 0 | 0 |

The per-arm means pool tasks unevenly (five tasks have two trials, three have one); the per-task tables and the
overhead ratios below weigh each task once.

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 1/1 · 30 ± 0 · 0.39 ± 0.00 · 1.0 ± 0.0 | 1/1 · 35 ± 0 · 0.20 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 2/2 · 353 ± 180 · 1.78 ± 0.26 · 2.5 ± 0.7 | 2/2 · 73 ± 7 · 0.37 ± 0.02 · 2.5 ± 2.1 |
| fix-off-by-one | 2/2 · 50 ± 7 · 0.51 ± 0.03 · 2.0 ± 0.0 | 2/2 · 19 ± 4 · 0.16 ± 0.00 · 1.0 ± 0.0 |
| long-backlog | 2/2 · 210 ± 49 · 3.76 ± 0.96 · 6.0 ± 1.4 | 2/2 · 143 ± 35 · 0.66 ± 0.11 · 3.0 ± 1.4 |
| multi-repo-currency | 1/1 · 135 ± 0 · 1.63 ± 0.00 · 4.0 ± 0.0 | 1/1 · 98 ± 0 · 0.52 ± 0.00 · 8.0 ± 0.0 |
| multi-repo-pagination | 2/2 · 145 ± 21 · 1.56 ± 0.02 · 4.0 ± 0.0 | 2/2 · 103 ± 9 · 0.52 ± 0.06 · 5.0 ± 0.0 |
| refactor-order-status | 2/2 · 163 ± 25 · 1.22 ± 0.21 · 2.5 ± 0.7 | 2/2 · 61 ± 3 · 0.38 ± 0.01 · 2.0 ± 0.0 |
| review-seeded-bug | 1/1 · 45 ± 0 · 0.30 ± 0.00 · 1.0 ± 0.0 | 1/1 · 40 ± 0 · 0.22 ± 0.00 · 1.0 ± 0.0 |

## Archon's overhead against the aether arm, per task

Ratios of the per-task means, archon ÷ aether. "Orchestrator $" is the trial's cost minus what its queue items
spent (`archon-items.json`), i.e. what the orchestrator agent itself cost.

| Task | Wall × | Input tok × | Cost × | Orchestrator $ | Items | Pipelines the orchestrator chose |
|---|---:|---:|---:|---:|---:|---|
| ask-cache-invalidation | 0.86 | 1.28 | 1.91 | 0.39 | 0 | answered itself |
| feature-token-bucket | 4.86 | 7.30 | 4.76 | 0.44 | 1, 2 | pr-review (review failed once, repaired); pr-review + direct follow-up |
| fix-off-by-one | 2.63 | 5.04 | 3.22 | 0.34 | 1, 1 | direct |
| long-backlog | 1.47 | 5.55 | 5.73 | 0.76 | 4, 4 | 4 × pr-review; 4 × direct with a reviewer, 2 run twice |
| multi-repo-currency | 1.38 | 3.06 | 3.17 | 0.47 | 2 | 2 × pr-review |
| multi-repo-pagination | 1.41 | 2.69 | 2.99 | 0.46 | 2, 2 | 2 × pr-review |
| refactor-order-status | 2.69 | 4.49 | 3.24 | 0.46 | 1, 2 | pr-review; pr-review + direct follow-up |
| review-seeded-bug | 1.13 | 2.62 | 1.37 | 0.30 | 0 | answered itself |
| **All 8 (sum of task means)** | **1.98** | **4.20** | **3.69** | 30% of archon $ | | |

## Compared with results-v1

The seven tasks results-v1 has an archon cell for (it has none for `review-seeded-bug`), each task weighed once
(sum of per-task means). v1: pre-1.0 Archon on aetherd 0.1.1, three trials planned per cell, 1–2 graded.

| Metric | v1 archon | v2 archon | change | v1 aether | v2 aether | change | Overhead v1 | Overhead v2 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Pass rate | 12/12 | 13/13 | = | 13/13 | 13/13 | = | — | — |
| Wall s | 1965 | 1086 | −45% | 651 | 531 | −19% | 3.02× | 2.05× |
| Input tokens | 9.44 M | 7.68 M | −19% | 2.03 M | 1.79 M | −12% | 4.65× | 4.29× |
| Cost $ | 14.17 | 10.86 | −23% | 3.01 | 2.81 | −7% | 4.70× | 3.87× |

Per task, archon (v1 → v2) / aether (v1 → v2):

| Task | Wall s | Input k tok | Cost $ |
|---|---|---|---|
| ask-cache-invalidation | 35 → 30 / 34 → 35 | 148 → 119 / 102 → 93 | 0.23 → 0.39 / 0.20 → 0.20 |
| feature-token-bucket | 586 → 353 / 109 → 73 | 2011 → 1413 / 330 → 194 | 3.04 → 1.78 / 0.51 → 0.37 |
| fix-off-by-one | 182 → 50 / 45 → 19 | 500 → 452 / 87 → 90 | 0.64 → 0.51 / 0.16 → 0.16 |
| long-backlog | 390 → 210 / 165 → 143 | 2724 → 2753 / 516 → 496 | 3.97 → 3.76 / 0.72 → 0.66 |
| multi-repo-currency | 193 → 135 / 123 → 98 | 1384 → 1059 / 394 → 347 | 2.19 → 1.63 / 0.56 → 0.52 |
| multi-repo-pagination | 388 → 145 / 111 → 103 | 1837 → 1018 / 375 → 379 | 2.73 → 1.56 / 0.49 → 0.52 |
| refactor-order-status | 190 → 163 / 64 → 61 | 839 → 866 / 226 → 193 | 1.36 → 1.22 / 0.38 → 0.38 |
| review-seeded-bug | — → 45 / 55 → 40 | — → 242 / 115 → 92 | — → 0.30 / 0.27 → 0.22 |

## Verdict

At equal quality (every trial of both arms passed its hidden grader), Archon 1.1.0 is faster and cheaper than the
pre-1.0 build of results-v1, but it is **not** fast and cheap relative to plain Aether: on the same model it takes
about 2× the wall time and about 4× the input tokens and 3.7× the dollars. Only on questions it answers itself
(`ask-cache-invalidation`, `review-seeded-bug`) is it near parity (0.9–1.1× wall, 1.4–1.9× cost). The overhead
has two sources: the orchestrator's own turns (30% of Archon's spend, $0.30–0.76 a trial), and pipelines that add
a separate review agent, a repair round when review fails, and re-runs (a whole run again when an implementer
amends its commit after its reviewer read it, which the 1.1.0 review binding refuses to ship). The aether arm does
none of these, and on this suite the grader never rewarded them.

## Caveats

- n is 1–2 per cell; the stdevs show wide spread for Archon (`feature-token-bucket` 225 s vs 480 s, depending on
  whether review failed the first attempt). Treat the ratios as a direction, not a precise figure.
- The arms ran on a private sandbox aetherd (1.0.3, its own config under `~/.local/share/archon-bench/lf`, empty
  approval/network/scheduler sections, `max_parallel_agents` default), not the operator's daemon. Archon ran in
  shared mode on it with the 1.1.0 `archon setup --print` MCP registration.
- v1 and v2 differ in more than Archon: aetherd 0.1.1 → 1.0.3 and a newer Claude Code. The aether arm improved
  too (−7% cost, −19% wall), so the like-for-like measure of Archon's change is the overhead ratio
  (cost 4.70× → 3.87×, wall 3.02× → 2.05×), not the raw archon numbers.
- `review-seeded-bug` has no v1 archon cell (an infrastructure failure there), so it is left out of the v1
  comparison.
- The harness `summary.md` (`harness-summary.md`, rendered by `archon-bench report` from the merged
  `results.jsonl`) says "trials per task and arm: 2"; three tasks have one. Round 2's trials are renumbered
  `trial = 2` in the merged file; the raw per-round files under `raw/` are untouched.
- Wall time excludes Archon's daemon start-up and the 45 s quiet window the harness uses to detect that Archon
  finished, as in v1.

## Harness changes for 1.1.0

None. `archon-bench verify` passed every grader, `--dry-run` showed the expected command lines, and both rounds
ran with no infrastructure failure using the 1.1.0 binary (`--archon-bin`) on a sandbox aetherd (`--aether-socket`).

## Reproduce

```sh
cargo build --release -p archon-bench
archon-bench run --arms archon,aether --trials 1 --parallel 2 --effort high \
  --model claude-code/claude-opus-5-5 --archon-bin <archon 1.1.0> --aether-socket <aetherd.sock> --max-usd 38
archon-bench run --arms archon,aether --trials 1 --parallel 2 --effort high \
  --model claude-code/claude-opus-5-5 --archon-bin <archon 1.1.0> --aether-socket <aetherd.sock> --max-usd 20 \
  --only feature-token-bucket,long-backlog,fix-off-by-one,multi-repo-pagination,refactor-order-status
```

## Files

- `results.jsonl`, `run.json`, `chart.json`, `report.html`, `harness-summary.md`: both rounds merged, re-rendered
  with `archon-bench report`.
- `raw/round1-20261014T071033Z/`, `raw/round2-20261014T073720Z/`: each run's own output as the harness wrote it.

The method is the harness's own, as in results-v1: same prompt, effort, time limit and model for both arms;
permission prompts bypassed; fresh fixture copies outside the checkout; hidden graders applied after every agent
process stopped; costs as billed by the provider through Aether.
