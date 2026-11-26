# Archon benchmark

- Run `20261126T021333Z`: 2026-11-26T02:13:33.015666582+00:00 → 2026-11-26T02:17:14.207226612+00:00
- Archon commit: `421720e67fa55670e84ed89458b289e91c363f0a`
- Effort: `medium`; trials per task and arm: 1; parallel: 1
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.4.3`
- Version `archon`: `archon 1.6.5`
- Trials: 1 planned, 1 completed, 0 skipped; spent $0.04 of $5.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 1 | 1/1 (100%) | 100% | 100% | 1.00 ± 0.00 | 175 ± 0 | 1183342 ± 0 | 23164 ± 0 | 0.04 ± 0.00 | 2.0 ± 0.0 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon |
|---|---|
| quality-retry-budget | 1/1 · 175 ± 0 · 0.04 ± 0.00 · 2.0 ± 0.0 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| direct | 1 | 1/1 (100%) | 175 ± 0 | 0.04 ± 0.00 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| standard | 1 | 1/1 (100%) | 175 ± 0 | 0.04 ± 0.00 | 0 | 0 |

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
