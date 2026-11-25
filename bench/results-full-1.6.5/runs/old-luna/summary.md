# Archon benchmark

- Run `20261125T004922Z`: 2026-11-25T00:49:22.356839858+00:00 → 2026-11-25T04:01:29.980257218+00:00
- Archon commit: `c4bcaa9f8eff80ca81917f9ddaf65a866abb3690`
- Effort: `medium`; trials per task and arm: 3; parallel: 1
- Model `aether`: `codex/gpt-6-luna`
- Model `archon`: `codex/gpt-6-luna`
- Version `aetherd`: `1.4.3`
- Version `archon`: `archon 1.6.4`
- Trials: 66 planned, 66 completed, 0 skipped; spent * $1.06 of $30.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `codex/gpt-6-luna` | 33 | 29/33 (88%) | 88% | 100% | 0.92 ± 0.25 | 240 ± 149 | 1122121 ± 772969 | 11307 ± 8123 | * 0.03 ± 0.02 | 3.0 ± 1.7 | 0 | 0 |
| aether | `codex/gpt-6-luna` | 33 | 30/33 (91%) | 91% | 100% | 0.93 ± 0.25 | 63 ± 33 | 137881 ± 68662 | 2575 ± 1549 | 0.00 ± 0.00 | 3.6 ± 1.1 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 40 ± 5 · 0.01 ± 0.00 · 1.7 ± 1.2 | 2/3 · 20 ± 5 · 0.00 ± 0.00 · 2.0 ± 0.0 |
| feature-token-bucket | 3/3 · 290 ± 102 · * 0.03 ± 0.01 · 2.0 ± 0.0 | 3/3 · 45 ± 0 · 0.00 ± 0.00 · 3.0 ± 0.0 |
| fix-off-by-one | 3/3 · 154 ± 49 · * 0.01 ± 0.00 · 2.0 ± 0.0 | 3/3 · 28 ± 3 · 0.00 ± 0.00 · 3.0 ± 0.0 |
| long-backlog | 1/3 · 375 ± 196 · * 0.07 ± 0.01 · 6.0 ± 0.0 | 3/3 · 127 ± 15 · 0.01 ± 0.00 · 5.0 ± 0.0 |
| multi-repo-currency | 2/3 · 217 ± 75 · 0.03 ± 0.01 · 5.7 ± 0.6 | 1/3 · 103 ± 20 · 0.01 ± 0.00 · 5.3 ± 1.5 |
| multi-repo-pagination | 3/3 · 392 ± 129 · * 0.04 ± 0.02 · 4.7 ± 0.6 | 3/3 · 78 ± 8 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| quality-ledger-transfer | 3/3 · 314 ± 149 · * 0.03 ± 0.01 · 2.3 ± 0.6 | 3/3 · 72 ± 13 · 0.00 ± 0.00 · 4.3 ± 0.6 |
| quality-retry-budget | 2/3 · 167 ± 55 · * 0.02 ± 0.01 · 2.0 ± 0.0 | 3/3 · 52 ± 3 · 0.00 ± 0.00 · 3.0 ± 0.0 |
| quality-safe-extract | 3/3 · 277 ± 50 · * 0.03 ± 0.01 · 3.0 ± 1.7 | 3/3 · 73 ± 6 · 0.00 ± 0.00 · 3.7 ± 0.6 |
| refactor-order-status | 3/3 · 373 ± 101 · * 0.04 ± 0.01 · 3.0 ± 0.0 | 3/3 · 70 ± 13 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| review-seeded-bug | 3/3 · 37 ± 3 · 0.00 ± 0.00 · 1.0 ± 0.0 | 3/3 · 28 ± 3 · 0.00 ± 0.00 · 2.0 ± 0.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 1 | 1/1 (100%) | 280 ± 0 | * 0.03 ± 0.00 | 0 | 0 |
| direct | 24 | 20/24 (83%) | 273 ± 126 | * 0.03 ± 0.02 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 38 ± 4 | 0.00 ± 0.00 | 0 | 0 |
| refactor | 2 | 2/2 (100%) | 419 ± 87 | * 0.04 ± 0.01 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| no work (empty queue) | 6 | 6/6 (100%) | 38 ± 4 | 0.00 ± 0.00 | 0 | 0 |
| none | 4 | 2/4 (50%) | 189 ± 83 | 0.02 ± 0.01 | 0 | 0 |
| standard | 19 | 17/19 (89%) | 293 ± 131 | * 0.04 ± 0.02 | 0 | 0 |
| strict | 4 | 4/4 (100%) | 337 ± 109 | * 0.03 ± 0.01 | 0 | 0 |

## Method

- Every arm gets the same task prompt, effort and time limit, and the same model where the harness allows; `codex` runs its own model. Every arm runs with its tool permission prompts bypassed.
- `claude` keeps Claude Code's built-in subagents (the Task tool); the `aether` arm is a single leaf agent without delegation.
- Each trial starts from a fresh copy of the fixtures in an unguessably named directory outside the Archon checkout, with a one-commit git history. Hidden tests, reference solutions and rubrics are copied in only at grade time, into a separate copy the agent never sees, after the agent and every process it started have stopped. Agent processes do not inherit environment variables pointing at the checkout. There is no OS sandbox: an agent that searched the whole home directory could still find the checkout.
- Success is the hidden grader's verdict. Test graders restore the visible tests from the task, not from the agent's repository, and require the runner's own summary to report no failures and a minimum number of passing tests. Rubric graders need related facts close together and cap the answer length.
- A failed trial with a harness error (the arm could not run cleanly: a provider limit, a crash, a grading failure) is an infrastructure failure. It is left out of pass rates and every metric, counted under Infra and listed with its error.
- Wall time runs from handing over the prompt until the arm finished; Archon's daemon start-up (recorded as `setup_secs`) and the quiet window used to detect that Archon finished are excluded.
- Input tokens include cache reads and writes. Cost is the API-equivalent total: billed dollars plus Aether's estimate of unbilled usage (such as Codex), or `--price` list prices for an arm that reports neither. `(n=…)` marks a metric only that many trials reported. `≥` marks a cost Aether reported as incomplete (`cost_gap`); the true cost is at least that. An agent's estimate prices its own unpriced usage, so only unpriced work no estimate covers counts as incomplete. `*` marks a cost that includes one Aether reported as incomplete together with estimates; it is not a bound.
- Operator messages are what the operator had to read: one final result for `claude`/`codex`, the assistant messages of the Aether agent, and the default-visible chat lines for Archon.
- Values are mean ± sample standard deviation over graded trials. pass@k uses the unbiased estimator per task, averaged over tasks, with k the fewest graded trials any (task, arm) cell has.
