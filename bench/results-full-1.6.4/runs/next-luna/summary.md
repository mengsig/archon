# Archon benchmark

- Run `20261120T085850Z`: 2026-11-20T08:58:50.716709441+00:00 → 2026-11-20T12:21:54.287119632+00:00
- Archon commit: `c17af3feba8f5a6a2adf480d77d4477894d6eccc`
- Effort: `medium`; trials per task and arm: 3; parallel: 1
- Model `aether`: `codex/gpt-6-luna`
- Model `archon`: `codex/gpt-6-luna`
- Version `aetherd`: `1.4.2`
- Version `archon`: `archon 1.6.4`
- Trials: 66 planned, 66 completed, 0 skipped; spent * $1.04 of $38.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `codex/gpt-6-luna` | 33 | 24/33 (73%) | 73% | 82% | 0.77 ± 0.41 | 262 ± 306 | 1123788 ± 768664 | 11771 ± 8688 | * 0.03 ± 0.02 | 2.7 ± 1.4 | 0 | 1 |
| aether | `codex/gpt-6-luna` | 33 | 26/33 (79%) | 79% | 82% | 0.80 ± 0.39 | 61 ± 33 | 144614 ± 74180 | 2510 ± 1468 | 0.00 ± 0.00 | 3.6 ± 1.0 | 0 | 0 |

## Graded on timeout

A trial that hit its deadline keeps the grader verdict and score of its succeeded items only; what its cancelled items had committed counts for nothing in the success, pass@k and score columns. Here the last committed branch of each timed-out Archon item was graded as well, which shows a slow but correct solution. These results are never counted as passes.

| Arm | Timeouts | Graded on timeout | Passed when graded on timeout |
|---|---:|---:|---:|
| archon | 1 | 1 | 1 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 28 ± 3 · 0.00 ± 0.00 · 1.0 ± 0.0 | 3/3 · 18 ± 3 · 0.00 ± 0.00 · 2.0 ± 0.0 |
| feature-token-bucket | 3/3 · 158 ± 39 · * 0.02 ± 0.00 · 2.0 ± 0.0 | 3/3 · 45 ± 5 · 0.00 ± 0.00 · 3.3 ± 0.6 |
| fix-off-by-one | 3/3 · 212 ± 110 · * 0.02 ± 0.01 · 2.3 ± 0.6 | 3/3 · 30 ± 5 · 0.00 ± 0.00 · 3.0 ± 0.0 |
| long-backlog | 1/3 · 288 ± 101 · * 0.06 ± 0.01 · 5.0 ± 0.0 | 3/3 · 98 ± 16 · 0.01 ± 0.00 · 4.7 ± 0.6 |
| multi-repo-currency | 0/3 · 374 ± 129 · * 0.04 ± 0.01 · 5.0 ± 0.0 | 0/3 · 117 ± 42 · 0.01 ± 0.00 · 5.3 ± 0.6 |
| multi-repo-pagination | 2/3 · 749 ± 912 · * 0.05 ± 0.01 · 4.0 ± 0.0 | 3/3 · 79 ± 16 · 0.00 ± 0.00 · 3.7 ± 0.6 |
| quality-ledger-transfer | 3/3 · 217 ± 54 · * 0.02 ± 0.00 · 2.3 ± 0.6 | 3/3 · 57 ± 10 · 0.00 ± 0.00 · 3.7 ± 0.6 |
| quality-retry-budget | 3/3 · 172 ± 17 · * 0.02 ± 0.00 · 2.0 ± 0.0 | 3/3 · 72 ± 13 · 0.00 ± 0.00 · 3.7 ± 0.6 |
| quality-safe-extract | 3/3 · 251 ± 44 · * 0.03 ± 0.00 · 2.3 ± 0.6 | 3/3 · 82 ± 21 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| refactor-order-status | 0/3 · 390 ± 168 · * 0.03 ± 0.01 · 3.0 ± 0.0 | 0/3 · 55 ± 5 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| review-seeded-bug | 3/3 · 43 ± 3 · 0.01 ± 0.00 · 1.0 ± 0.0 | 2/3 · 23 ± 3 · 0.00 ± 0.00 · 2.0 ± 0.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 4 | 3/4 (75%) | 297 ± 132 | * 0.03 ± 0.01 | 0 | 0 |
| direct | 21 | 15/21 (71%) | 300 ± 353 | * 0.03 ± 0.02 | 0 | 1 |
| no work (empty queue) | 6 | 6/6 (100%) | 36 ± 9 | 0.00 ± 0.00 | 0 | 0 |
| refactor | 2 | 0/2 (0%) | 475 ± 118 | * 0.04 ± 0.01 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: standard + strict | 1 | 0/1 (0%) | 1802 ± 0 | * 0.06 ± 0.00 | 0 | 1 |
| no work (empty queue) | 6 | 6/6 (100%) | 36 ± 9 | 0.00 ± 0.00 | 0 | 0 |
| none | 2 | 2/2 (100%) | 141 ± 35 | 0.02 ± 0.00 | 0 | 0 |
| standard | 19 | 13/19 (68%) | 247 ± 95 | * 0.03 ± 0.02 | 0 | 0 |
| strict | 5 | 3/5 (60%) | 332 ± 150 | * 0.03 ± 0.01 | 0 | 0 |

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
