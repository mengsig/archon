# Archon benchmark

- Run `20261015T064316Z`: 2026-10-15T06:43:16.885499055+00:00 → 2026-10-15T07:12:16.337519915+00:00
- Archon commit: `1751ed72933ad137025605726f0c6dc9eb5ae0d2`
- Effort: `high`; trials per task and arm: 1; parallel: 1
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.0.6`
- Version `archon`: `archon 1.2.2`
- Trials: 8 planned, 8 completed, 0 skipped; spent ≥ $0.32 of $7.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 4 | 4/4 (100%) | 100% | 100% | 1.00 ± 0.00 | 289 ± 319 | 1657702 ± 1851627 | 47316 ± 62170 | ≥ 0.07 ± 0.07 | 2.0 ± 0.8 | 0 | 0 |
| aether | `claude-code/claude-haiku-5-5` | 4 | 4/4 (100%) | 100% | 100% | 1.00 ± 0.00 | 82 ± 67 | 290248 ± 159968 | 9328 ± 5098 | ≥ 0.01 ± 0.00 | 4.8 ± 3.1 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| fix-off-by-one | 1/1 · 65 ± 0 · ≥ 0.02 ± 0.00 · 2.0 ± 0.0 | 1/1 · 25 ± 0 · 0.01 ± 0.00 · 3.0 ± 0.0 |
| quality-ledger-transfer | 1/1 · 762 ± 0 · ≥ 0.17 ± 0.00 · 2.0 ± 0.0 | 1/1 · 80 ± 0 · 0.02 ± 0.00 · 9.0 ± 0.0 |
| quality-retry-budget | 1/1 · 185 ± 0 · ≥ 0.06 ± 0.00 · 3.0 ± 0.0 | 1/1 · 176 ± 0 · ≥ 0.01 ± 0.00 · 5.0 ± 0.0 |
| review-seeded-bug | 1/1 · 146 ± 0 · 0.01 ± 0.00 · 1.0 ± 0.0 | 1/1 · 45 ± 0 · 0.01 ± 0.00 · 2.0 ± 0.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| direct | 3 | 3/3 (100%) | 337 ± 373 | ≥ 0.09 ± 0.08 | 0 | 0 |
| no work (empty queue) | 1 | 1/1 (100%) | 146 ± 0 | 0.01 ± 0.00 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| no work (empty queue) | 1 | 1/1 (100%) | 146 ± 0 | 0.01 ± 0.00 | 0 | 0 |
| standard | 2 | 2/2 (100%) | 125 ± 85 | ≥ 0.04 ± 0.02 | 0 | 0 |
| strict | 1 | 1/1 (100%) | 762 ± 0 | ≥ 0.17 ± 0.00 | 0 | 0 |

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
