# Archon benchmark

- Run `20261015T072502Z`: 2026-10-15T07:25:02.425783041+00:00 → 2026-10-15T07:43:00.836372537+00:00
- Archon commit: `17eeadcba1bbd56e40659527e1b4afa2417f6bf8`
- Effort: `medium`; trials per task and arm: 1; parallel: 1
- Model `aether`: `codex/gpt-6-luna`
- Model `archon`: `codex/gpt-6-luna`
- Version `aetherd`: `1.0.6`
- Version `archon`: `archon 1.2.2`
- Trials: 8 planned, 8 completed, 0 skipped; spent ≥ $0.12 of $7.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `codex/gpt-6-luna` | 4 | 2/4 (50%) | 50% | 50% | 0.62 ± 0.48 | 183 ± 135 | 1063796 ± 765728 | 9808 ± 8041 | ≥ 0.03 ± 0.02 | 3.0 ± 1.6 | 0 | 0 |
| aether | `codex/gpt-6-luna` | 4 | 3/4 (75%) | 75% | 75% | 0.88 ± 0.25 | 34 ± 12 | 123638 ± 51630 | 1804 ± 1179 | 0.00 ± 0.00 | 3.2 ± 1.0 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| fix-off-by-one | 1/1 · 150 ± 0 · ≥ 0.02 ± 0.00 · 3.0 ± 0.0 | 1/1 · 25 ± 0 · 0.00 ± 0.00 · 3.0 ± 0.0 |
| quality-ledger-transfer | 0/1 · 350 ± 0 · ≥ 0.05 ± 0.00 · 5.0 ± 0.0 | 1/1 · 35 ± 0 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| quality-retry-budget | 1/1 · 205 ± 0 · ≥ 0.03 ± 0.00 · 3.0 ± 0.0 | 1/1 · 50 ± 0 · 0.00 ± 0.00 · 4.0 ± 0.0 |
| review-seeded-bug | 0/1 · 25 ± 0 · 0.00 ± 0.00 · 1.0 ± 0.0 | 0/1 · 25 ± 0 · 0.00 ± 0.00 · 2.0 ± 0.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| direct | 3 | 2/3 (67%) | 235 ± 103 | ≥ 0.03 ± 0.01 | 0 | 0 |
| no work (empty queue) | 1 | 0/1 (0%) | 25 ± 0 | 0.00 ± 0.00 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: none + standard | 2 | 2/2 (100%) | 178 ± 39 | ≥ 0.03 ± 0.01 | 0 | 0 |
| no work (empty queue) | 1 | 0/1 (0%) | 25 ± 0 | 0.00 ± 0.00 | 0 | 0 |
| standard | 1 | 0/1 (0%) | 350 ± 0 | ≥ 0.05 ± 0.00 | 0 | 0 |

## Method

- Every arm gets the same task prompt, effort and time limit, and the same model where the harness allows; `codex` runs its own model. Every arm runs with its tool permission prompts bypassed.
- `claude` keeps Claude Code's built-in subagents (the Task tool); the `aether` arm is a single leaf agent without delegation.
- Each trial starts from a fresh copy of the fixtures in an unguessably named directory outside the Archon checkout, with a one-commit git history. Hidden tests, reference solutions and rubrics are copied in only at grade time, into a separate copy the agent never sees, after the agent and every process it started have stopped. Agent processes do not inherit environment variables pointing at the checkout. There is no OS sandbox: an agent that searched the whole home directory could still find the checkout.
- Success is the hidden grader's verdict. Test graders restore the visible tests from the task, not from the agent's repository, and require the runner's own summary to report no failures and a minimum number of passing tests. Rubric graders need related facts close together and cap the answer length.
- A failed trial with a harness error (the arm could not run cleanly: a provider limit, a crash, a grading failure) is an infrastructure failure. It is left out of pass rates and every metric, counted under Infra and listed with its error.
- Wall time runs from handing over the prompt until the arm finished; Archon's daemon start-up (recorded as `setup_secs`) and the quiet window used to detect that Archon finished are excluded.
- Input tokens include cache reads and writes. Cost is billed dollars when reported, else Aether's estimate or `--price` list prices. `(n=…)` marks a metric only that many trials reported. `≥` marks a cost Aether reported as incomplete (`cost_gap`); the true cost is at least that. `*` marks a cost that includes one Aether reported as incomplete together with estimates; it is not a bound.
- Operator messages are what the operator had to read: one final result for `claude`/`codex`, the assistant messages of the Aether agent, and the default-visible chat lines for Archon.
- Values are mean ± sample standard deviation over graded trials. pass@k uses the unbiased estimator per task, averaged over tasks, with k the fewest graded trials any (task, arm) cell has.
