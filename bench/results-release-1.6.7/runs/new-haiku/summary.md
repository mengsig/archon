# Archon benchmark

- Run `20261128T072018Z`: 2026-11-28T07:20:18.624686252+00:00 → 2026-11-28T10:04:57.359826171+00:00
- Archon commit: `acaad050ea30db9449bfb3821173d76d5a87bf57`
- Effort: `high`; trials per task and arm: 3; parallel: 3
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.4.4`
- Version `archon`: `archon 1.6.7`
- Trials: 66 planned, 66 completed, 0 skipped; spent ≥ $3.86 of $37.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 33 | 32/33 (97%) | 97% | 100% | 0.97 ± 0.17 | 250 ± 230 | 2366250 ± 1680794 | 61213 ± 50610 | ≥ 0.10 ± 0.08 | 2.8 ± 1.3 | 0 | 0 |
| aether | `claude-code/claude-haiku-5-5` | 33 | 32/33 (97%) | 97% | 100% | 0.97 ± 0.17 | 83 ± 43 | 482399 ± 332805 | 16212 ± 8694 | 0.02 ± 0.01 | 7.7 ± 4.7 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 39 ± 8 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 38 ± 4 · 0.01 ± 0.00 · 2.7 ± 0.6 |
| feature-token-bucket | 3/3 · 303 ± 178 · 0.08 ± 0.04 · 2.7 ± 0.6 | 3/3 · 80 ± 26 · 0.02 ± 0.00 · 5.7 ± 2.1 |
| fix-off-by-one | 3/3 · 95 ± 8 · 0.03 ± 0.00 · 2.0 ± 0.0 | 3/3 · 24 ± 5 · 0.01 ± 0.00 · 4.3 ± 1.2 |
| long-backlog | 3/3 · 185 ± 45 · 0.14 ± 0.01 · 5.0 ± 0.0 | 3/3 · 153 ± 12 · 0.03 ± 0.00 · 10.3 ± 1.5 |
| multi-repo-currency | 3/3 · 178 ± 7 · ≥ 0.09 ± 0.01 · 5.0 ± 0.0 | 3/3 · 124 ± 40 · 0.03 ± 0.01 · 15.7 ± 4.5 |
| multi-repo-pagination | 3/3 · 229 ± 73 · 0.12 ± 0.04 · 4.0 ± 0.0 | 2/3 · 110 ± 5 · 0.03 ± 0.00 · 15.7 ± 1.2 |
| quality-ledger-transfer | 3/3 · 364 ± 181 · 0.15 ± 0.11 · 2.3 ± 0.6 | 3/3 · 56 ± 4 · 0.01 ± 0.00 · 4.7 ± 0.6 |
| quality-retry-budget | 3/3 · 132 ± 25 · 0.04 ± 0.01 · 2.0 ± 0.0 | 3/3 · 52 ± 8 · 0.01 ± 0.00 · 6.3 ± 1.5 |
| quality-safe-extract | 3/3 · 439 ± 114 · 0.19 ± 0.06 · 2.0 ± 0.0 | 3/3 · 127 ± 10 · 0.03 ± 0.00 · 7.0 ± 1.0 |
| refactor-order-status | 3/3 · 739 ± 354 · 0.20 ± 0.09 · 3.0 ± 0.0 | 3/3 · 99 ± 11 · 0.02 ± 0.00 · 9.3 ± 0.6 |
| review-seeded-bug | 2/3 · 50 ± 13 · ≥ 0.02 ± 0.01 · 1.3 ± 0.6 | 3/3 · 53 ± 8 · 0.01 ± 0.00 · 3.0 ± 1.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 4 | 4/4 (100%) | 394 ± 129 | 0.16 ± 0.07 | 0 | 0 |
| direct | 19 | 19/19 (100%) | 191 ± 96 | ≥ 0.08 ± 0.04 | 0 | 0 |
| mixed: adaptive + direct | 1 | 1/1 (100%) | 573 ± 0 | 0.28 ± 0.00 | 0 | 0 |
| no work (empty queue) | 6 | 5/6 (83%) | 44 ± 12 | ≥ 0.01 ± 0.00 | 0 | 0 |
| refactor | 3 | 3/3 (100%) | 739 ± 354 | 0.20 ± 0.09 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: standard + strict | 3 | 3/3 (100%) | 369 ± 178 | 0.18 ± 0.08 | 0 | 0 |
| no work (empty queue) | 6 | 5/6 (83%) | 44 ± 12 | ≥ 0.01 ± 0.00 | 0 | 0 |
| standard | 17 | 17/17 (100%) | 182 ± 97 | ≥ 0.08 ± 0.04 | 0 | 0 |
| strict | 7 | 7/7 (100%) | 542 ± 290 | 0.18 ± 0.07 | 0 | 0 |

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
