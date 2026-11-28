# Archon benchmark

- Run `20261128T072014Z`: 2026-11-28T07:20:14.601655583+00:00 → 2026-11-28T10:44:48.558365917+00:00
- Archon commit: `acaad050ea30db9449bfb3821173d76d5a87bf57`
- Effort: `high`; trials per task and arm: 3; parallel: 3
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.4.3`
- Version `archon`: `archon 1.6.6`
- Trials: 66 planned, 66 completed, 0 skipped; spent ≥ $5.07 of $37.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 33 | 29/33 (88%) | 88% | 91% | 0.88 ± 0.33 | 328 ± 308 | 2935507 ± 2154691 | 77219 ± 68017 | ≥ 0.13 ± 0.13 | 2.8 ± 1.8 | 0 | 4 |
| aether | `claude-code/claude-haiku-5-5` | 33 | 33/33 (100%) | 100% | 100% | 1.00 ± 0.00 | 83 ± 42 | 489593 ± 325232 | 16418 ± 8963 | 0.02 ± 0.01 | 7.9 ± 5.3 | 0 | 0 |

## Graded on timeout

A trial that hit its deadline keeps the grader verdict and score of its succeeded items only; what its cancelled items had committed counts for nothing in the success, pass@k and score columns. Here the last committed branch of each timed-out Archon item was graded as well, which shows a slow but correct solution. These results are never counted as passes.

| Arm | Timeouts | Graded on timeout | Passed when graded on timeout |
|---|---:|---:|---:|
| archon | 4 | 4 | 4 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 36 ± 2 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 27 ± 4 · 0.01 ± 0.00 · 2.0 ± 0.0 |
| feature-token-bucket | 3/3 · 302 ± 150 · 0.09 ± 0.05 · 2.3 ± 0.6 | 3/3 · 106 ± 44 · 0.02 ± 0.01 · 11.0 ± 4.4 |
| fix-off-by-one | 3/3 · 91 ± 9 · 0.03 ± 0.00 · 2.0 ± 0.0 | 3/3 · 24 ± 7 · 0.01 ± 0.00 · 4.0 ± 1.0 |
| long-backlog | 3/3 · 213 ± 75 · 0.16 ± 0.02 · 5.0 ± 0.0 | 3/3 · 150 ± 13 · 0.03 ± 0.00 · 11.0 ± 4.6 |
| multi-repo-currency | 3/3 · 228 ± 40 · 0.12 ± 0.02 · 6.3 ± 2.3 | 3/3 · 112 ± 15 · 0.03 ± 0.00 · 14.0 ± 3.5 |
| multi-repo-pagination | 3/3 · 400 ± 134 · ≥ 0.20 ± 0.07 · 4.3 ± 0.6 | 3/3 · 119 ± 20 · 0.03 ± 0.00 · 17.3 ± 4.9 |
| quality-ledger-transfer | 2/3 · 831 ± 553 · ≥ 0.28 ± 0.19 · 2.0 ± 0.0 | 3/3 · 72 ± 13 · 0.02 ± 0.00 · 6.3 ± 3.2 |
| quality-retry-budget | 3/3 · 135 ± 18 · 0.04 ± 0.00 · 2.0 ± 0.0 | 3/3 · 57 ± 3 · 0.01 ± 0.00 · 6.3 ± 1.2 |
| quality-safe-extract | 0/3 · 726 ± 4 · 0.38 ± 0.04 · 2.0 ± 0.0 | 3/3 · 120 ± 15 · 0.03 ± 0.00 · 6.0 ± 2.0 |
| refactor-order-status | 3/3 · 592 ± 106 · 0.16 ± 0.03 · 3.0 ± 0.0 | 3/3 · 80 ± 5 · 0.02 ± 0.00 · 6.7 ± 1.2 |
| review-seeded-bug | 3/3 · 52 ± 3 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 51 ± 7 · 0.01 ± 0.00 · 2.0 ± 1.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 5 | 1/5 (20%) | 894 ± 238 | ≥ 0.38 ± 0.03 | 0 | 4 |
| direct | 19 | 19/19 (100%) | 227 ± 126 | ≥ 0.10 ± 0.07 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 44 ± 9 | 0.01 ± 0.00 | 0 | 0 |
| refactor | 3 | 3/3 (100%) | 592 ± 106 | 0.16 ± 0.03 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: standard + strict | 2 | 2/2 (100%) | 399 ± 189 | 0.19 ± 0.10 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 44 ± 9 | 0.01 ± 0.00 | 0 | 0 |
| standard | 16 | 16/16 (100%) | 194 ± 97 | 0.08 ± 0.05 | 0 | 0 |
| strict | 9 | 5/9 (56%) | 739 ± 262 | ≥ 0.29 ± 0.11 | 0 | 4 |

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
