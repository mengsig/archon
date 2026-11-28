# Archon benchmark

- Run `20261128T072020Z`: 2026-11-28T07:20:20.955056636+00:00 → 2026-11-28T10:33:45.696075684+00:00
- Archon commit: `acaad050ea30db9449bfb3821173d76d5a87bf57`
- Effort: `high`; trials per task and arm: 3; parallel: 3
- Model `aether`: `codex/gpt-6-luna`
- Model `archon`: `codex/gpt-6-luna`
- Version `aetherd`: `1.4.4`
- Version `archon`: `archon 1.6.7`
- Trials: 66 planned, 66 completed, 0 skipped; spent * $1.09 of $37.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `codex/gpt-6-luna` | 33 | 30/33 (91%) | 91% | 100% | 0.93 ± 0.24 | 299 ± 270 | 1215940 ± 1046892 | 13808 ± 12373 | * 0.03 ± 0.02 | 2.8 ± 1.5 | 0 | 1 |
| aether | `codex/gpt-6-luna` | 33 | 33/33 (100%) | 100% | 100% | 1.00 ± 0.00 | 91 ± 42 | 168323 ± 82293 | 3843 ± 2065 | 0.00 ± 0.00 | 4.0 ± 1.0 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 52 ± 17 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 41 ± 9 · 0.00 ± 0.00 · 2.7 ± 0.6 |
| feature-token-bucket | 3/3 · 263 ± 105 · * 0.03 ± 0.01 · 2.3 ± 0.6 | 3/3 · 104 ± 53 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| fix-off-by-one | 3/3 · 118 ± 35 · * 0.01 ± 0.00 · 2.0 ± 0.0 | 3/3 · 45 ± 5 · 0.00 ± 0.00 · 3.3 ± 0.6 |
| long-backlog | 2/3 · 397 ± 197 · * 0.06 ± 0.00 · 5.0 ± 0.0 | 3/3 · 148 ± 12 · 0.01 ± 0.00 · 5.7 ± 1.5 |
| multi-repo-currency | 2/3 · 260 ± 90 · * 0.03 ± 0.01 · 5.0 ± 0.0 | 3/3 · 152 ± 6 · 0.01 ± 0.00 · 4.3 ± 0.6 |
| multi-repo-pagination | 3/3 · 479 ± 569 · * 0.05 ± 0.06 · 4.0 ± 0.0 | 3/3 · 127 ± 34 · 0.01 ± 0.00 · 4.7 ± 0.6 |
| quality-ledger-transfer | 2/3 · 661 ± 469 · * 0.02 ± 0.02 · 3.0 ± 1.7 | 3/3 · 80 ± 14 · 0.00 ± 0.00 · 3.7 ± 0.6 |
| quality-retry-budget | 3/3 · 184 ± 34 · * 0.02 ± 0.00 · 2.0 ± 0.0 | 3/3 · 77 ± 15 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| quality-safe-extract | 3/3 · 404 ± 128 · * 0.04 ± 0.01 · 2.0 ± 0.0 | 3/3 · 105 ± 10 · 0.01 ± 0.00 · 4.0 ± 0.0 |
| refactor-order-status | 3/3 · 399 ± 110 · * 0.03 ± 0.00 · 4.0 ± 1.0 | 3/3 · 73 ± 3 · 0.00 ± 0.00 · 4.3 ± 0.6 |
| review-seeded-bug | 3/3 · 67 ± 11 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 45 ± 5 · 0.00 ± 0.00 · 3.0 ± 0.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 4 | 4/4 (100%) | 398 ± 106 | * 0.04 ± 0.01 | 0 | 0 |
| direct | 22 | 19/22 (86%) | 335 ± 296 | * 0.03 ± 0.03 | 0 | 1 |
| no work (empty queue) | 6 | 6/6 (100%) | 60 ± 15 | 0.01 ± 0.00 | 0 | 0 |
| refactor | 1 | 1/1 (100%) | 525 ± 0 | * 0.03 ± 0.00 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| no work (empty queue) | 6 | 6/6 (100%) | 60 ± 15 | 0.01 ± 0.00 | 0 | 0 |
| none | 8 | 7/8 (88%) | 152 ± 42 | * 0.02 ± 0.01 | 0 | 0 |
| standard | 14 | 12/14 (86%) | 386 ± 261 | * 0.03 ± 0.02 | 0 | 1 |
| strict | 5 | 5/5 (100%) | 575 ± 331 | * 0.05 ± 0.04 | 0 | 0 |

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
