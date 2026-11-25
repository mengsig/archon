# Archon benchmark

- Run `20261120T123131Z`: 2026-11-20T12:31:31.260520+00:00 → 2026-11-20T13:17:54.106539599+00:00
- Archon commit: `c17af3feba8f5a6a2adf480d77d4477894d6eccc`
- Effort: `medium`; trials per task and arm: 3; parallel: 1
- Model `aether`: `codex/gpt-6-luna`
- Model `archon`: `codex/gpt-6-luna`
- Version `aetherd`: `1.4.2`
- Version `archon`: `archon 1.6.4`
- Trials: 12 planned, 12 completed, 0 skipped; spent * $0.25 of $25.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `codex/gpt-6-luna` | 6 | 6/6 (100%) | 100% | 100% | 1.00 ± 0.00 | 337 ± 119 | 1375626 ± 434591 | 16457 ± 4325 | * 0.04 ± 0.01 | 4.7 ± 1.5 | 0 | 0 |
| aether | `codex/gpt-6-luna` | 6 | 5/6 (83%) | 83% | 100% | 0.83 ± 0.41 | 78 ± 24 | 196388 ± 67546 | 3278 ± 1288 | 0.01 ± 0.00 | 3.8 ± 0.8 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| multi-repo-currency | 3/3 · 308 ± 151 · * 0.04 ± 0.01 · 5.0 ± 0.0 | 2/3 · 100 ± 0 · 0.01 ± 0.00 · 4.3 ± 0.6 |
| refactor-order-status | 3/3 · 366 ± 101 · * 0.03 ± 0.01 · 4.3 ± 2.3 | 3/3 · 57 ± 6 · 0.00 ± 0.00 · 3.3 ± 0.6 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| direct | 4 | 4/4 (100%) | 296 ± 125 | * 0.03 ± 0.01 | 0 | 0 |
| refactor | 2 | 2/2 (100%) | 419 ± 59 | * 0.04 ± 0.01 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| none | 2 | 2/2 (100%) | 208 ± 74 | 0.03 ± 0.00 | 0 | 0 |
| standard | 2 | 2/2 (100%) | 384 ± 103 | * 0.04 ± 0.01 | 0 | 0 |
| strict | 2 | 2/2 (100%) | 419 ± 59 | * 0.04 ± 0.01 | 0 | 0 |

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
