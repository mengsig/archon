# Archon benchmark

- Run `20261125T065314Z`: 2026-11-25T06:53:14.916342321+00:00 → 2026-11-25T09:29:45.455914376+00:00
- Archon commit: `3411735fba3f221e837213ef0259d82d89dfe6bd`
- Effort: `medium`; trials per task and arm: 3; parallel: 3
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.4.3`
- Version `archon`: `archon 1.6.5`
- Trials: 66 planned, 66 completed, 0 skipped; spent ≥ $3.41 of $28.77

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 33 | 29/33 (88%) | 88% | 100% | 0.88 ± 0.33 | 241 ± 193 | 2180078 ± 1492813 | 49030 ± 41541 | ≥ 0.09 ± 0.08 | 2.8 ± 1.5 | 0 | 2 |
| aether | `claude-code/claude-haiku-5-5` | 33 | 33/33 (100%) | 100% | 100% | 1.00 ± 0.00 | 63 ± 32 | 365066 ± 282674 | 10677 ± 6125 | 0.01 ± 0.01 | 5.7 ± 4.0 | 0 | 0 |

## Graded on timeout

A trial that hit its deadline keeps the grader verdict and score of its succeeded items only; what its cancelled items had committed counts for nothing in the success, pass@k and score columns. Here the last committed branch of each timed-out Archon item was graded as well, which shows a slow but correct solution. These results are never counted as passes.

| Arm | Timeouts | Graded on timeout | Passed when graded on timeout |
|---|---:|---:|---:|
| archon | 2 | 2 | 2 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 32 ± 3 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 27 ± 5 · 0.01 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 3/3 · 234 ± 154 · 0.07 ± 0.04 · 2.0 ± 0.0 | 3/3 · 52 ± 8 · 0.01 ± 0.00 · 5.0 ± 1.0 |
| fix-off-by-one | 3/3 · 85 ± 0 · 0.03 ± 0.00 · 2.0 ± 0.0 | 3/3 · 28 ± 6 · 0.01 ± 0.00 · 4.0 ± 1.0 |
| long-backlog | 3/3 · 204 ± 56 · 0.15 ± 0.03 · 5.0 ± 0.0 | 3/3 · 128 ± 14 · 0.03 ± 0.00 · 6.7 ± 2.9 |
| multi-repo-currency | 3/3 · 202 ± 55 · 0.09 ± 0.01 · 5.7 ± 1.2 | 3/3 · 95 ± 26 · 0.02 ± 0.01 · 12.7 ± 5.1 |
| multi-repo-pagination | 1/3 · 270 ± 145 · 0.12 ± 0.05 · 4.0 ± 0.0 | 3/3 · 88 ± 10 · 0.02 ± 0.00 · 12.3 ± 2.3 |
| quality-ledger-transfer | 3/3 · 392 ± 249 · ≥ 0.13 ± 0.10 · 2.3 ± 0.6 | 3/3 · 49 ± 14 · 0.01 ± 0.00 · 4.3 ± 1.5 |
| quality-retry-budget | 2/3 · 329 ± 340 · 0.04 ± 0.01 · 2.3 ± 0.6 | 3/3 · 49 ± 1 · 0.01 ± 0.00 · 4.7 ± 0.6 |
| quality-safe-extract | 2/3 · 483 ± 208 · ≥ 0.23 ± 0.11 · 2.0 ± 0.0 | 3/3 · 72 ± 8 · 0.02 ± 0.00 · 4.7 ± 1.2 |
| refactor-order-status | 3/3 · 369 ± 17 · 0.10 ± 0.00 · 3.3 ± 0.6 | 3/3 · 61 ± 9 · 0.01 ± 0.00 · 5.3 ± 1.5 |
| review-seeded-bug | 3/3 · 50 ± 11 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 38 ± 3 · 0.01 ± 0.00 · 1.7 ± 0.6 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 1 | 1/1 (100%) | 625 ± 0 | ≥ 0.23 ± 0.00 | 0 | 0 |
| direct | 24 | 20/24 (83%) | 264 ± 180 | ≥ 0.10 ± 0.08 | 0 | 2 |
| no work (empty queue) | 6 | 6/6 (100%) | 41 ± 12 | 0.01 ± 0.00 | 0 | 0 |
| refactor | 2 | 2/2 (100%) | 376 ± 16 | 0.10 ± 0.01 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: standard + strict | 1 | 1/1 (100%) | 271 ± 0 | 0.16 ± 0.00 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 41 ± 12 | 0.01 ± 0.00 | 0 | 0 |
| standard | 20 | 17/20 (85%) | 230 ± 161 | 0.08 ± 0.05 | 0 | 1 |
| strict | 6 | 5/6 (83%) | 471 ± 161 | ≥ 0.19 ± 0.10 | 0 | 1 |

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
