# Archon benchmark

- Run `20261120T084354Z`: 2026-11-20T08:43:54.461594038+00:00 → 2026-11-20T11:17:13.195038056+00:00
- Archon commit: `c17af3feba8f5a6a2adf480d77d4477894d6eccc`
- Effort: `medium`; trials per task and arm: 3; parallel: 1
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.4.2`
- Version `archon`: `archon 1.6.4`
- Trials: 66 planned, 66 completed, 0 skipped; spent ≥ $2.71 of $25.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 33 | 26/33 (79%) | 79% | 82% | 0.79 ± 0.42 | 173 ± 136 | 1788412 ± 1308638 | 40314 ± 35984 | ≥ 0.07 ± 0.05 | 3.3 ± 1.9 | 0 | 0 |
| aether | `claude-code/claude-haiku-5-5` | 33 | 27/33 (82%) | 82% | 82% | 0.82 ± 0.39 | 58 ± 31 | 383939 ± 269889 | 11873 ± 6769 | 0.02 ± 0.01 | 6.2 ± 4.1 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 32 ± 13 · 0.01 ± 0.00 · 1.7 ± 1.2 | 3/3 · 19 ± 1 · 0.01 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 3/3 · 154 ± 12 · 0.05 ± 0.00 · 2.7 ± 0.6 | 3/3 · 45 ± 5 · 0.01 ± 0.00 · 6.0 ± 1.0 |
| fix-off-by-one | 3/3 · 64 ± 10 · 0.03 ± 0.00 · 2.7 ± 0.6 | 3/3 · 16 ± 1 · 0.01 ± 0.00 · 3.3 ± 0.6 |
| long-backlog | 3/3 · 324 ± 119 · ≥ 0.14 ± 0.01 · 7.0 ± 2.0 | 3/3 · 113 ± 14 · 0.03 ± 0.00 · 8.0 ± 3.5 |
| multi-repo-currency | 0/3 · 151 ± 30 · 0.07 ± 0.01 · 5.7 ± 1.2 | 0/3 · 79 ± 10 · 0.02 ± 0.00 · 11.3 ± 4.2 |
| multi-repo-pagination | 2/3 · 264 ± 142 · 0.12 ± 0.02 · 4.0 ± 0.0 | 3/3 · 85 ± 25 · 0.02 ± 0.01 · 13.0 ± 6.1 |
| quality-ledger-transfer | 3/3 · 138 ± 41 · 0.05 ± 0.01 · 3.0 ± 1.0 | 3/3 · 45 ± 9 · 0.01 ± 0.00 · 5.0 ± 1.0 |
| quality-retry-budget | 3/3 · 125 ± 41 · 0.04 ± 0.01 · 2.3 ± 0.6 | 3/3 · 53 ± 21 · 0.01 ± 0.00 · 5.3 ± 0.6 |
| quality-safe-extract | 3/3 · 390 ± 214 · 0.14 ± 0.08 · 2.3 ± 0.6 | 3/3 · 88 ± 8 · 0.02 ± 0.00 · 6.0 ± 1.7 |
| refactor-order-status | 0/3 · 224 ± 91 · 0.07 ± 0.03 · 4.0 ± 1.0 | 0/3 · 61 ± 5 · 0.02 ± 0.00 · 6.3 ± 1.5 |
| review-seeded-bug | 3/3 · 35 ± 10 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 37 ± 3 · 0.01 ± 0.00 · 2.7 ± 1.2 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 1 | 1/1 (100%) | 252 ± 0 | 0.10 ± 0.00 | 0 | 0 |
| direct | 25 | 19/25 (76%) | 197 ± 135 | ≥ 0.08 ± 0.05 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 33 ± 10 | 0.01 ± 0.00 | 0 | 0 |
| refactor | 1 | 0/1 (0%) | 317 ± 0 | 0.10 ± 0.00 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: none + standard | 1 | 1/1 (100%) | 190 ± 0 | 0.14 ± 0.00 | 0 | 0 |
| mixed: standard + strict | 2 | 1/2 (50%) | 183 ± 32 | 0.11 ± 0.02 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 33 ± 10 | 0.01 ± 0.00 | 0 | 0 |
| none | 1 | 0/1 (0%) | 127 ± 0 | 0.05 ± 0.00 | 0 | 0 |
| standard | 18 | 14/18 (78%) | 162 ± 95 | ≥ 0.06 ± 0.03 | 0 | 0 |
| strict | 5 | 4/5 (80%) | 382 ± 156 | 0.13 ± 0.06 | 0 | 0 |

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
