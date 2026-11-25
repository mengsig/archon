# Archon benchmark

- Run `20261125T065314Z`: 2026-11-25T06:53:14.890457680+00:00 → 2026-11-25T08:47:19.878003575+00:00
- Archon commit: `3411735fba3f221e837213ef0259d82d89dfe6bd`
- Effort: `medium`; trials per task and arm: 3; parallel: 3
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `1.4.3`
- Version `archon`: `archon 1.6.4`
- Trials: 66 planned, 66 completed, 0 skipped; spent $2.54 of $28.77

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@3 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 33 | 32/33 (97%) | 97% | 100% | 0.97 ± 0.17 | 162 ± 104 | 1642793 ± 1174980 | 36920 ± 29687 | 0.06 ± 0.04 | 2.7 ± 1.4 | 0 | 0 |
| aether | `claude-code/claude-haiku-5-5` | 33 | 32/33 (97%) | 97% | 100% | 0.97 ± 0.17 | 62 ± 30 | 355824 ± 245834 | 11076 ± 5930 | 0.01 ± 0.01 | 6.3 ± 4.2 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 3/3 · 30 ± 0 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 28 ± 4 · 0.01 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 3/3 · 145 ± 13 · 0.04 ± 0.00 · 2.0 ± 0.0 | 3/3 · 49 ± 7 · 0.01 ± 0.00 · 4.7 ± 0.6 |
| fix-off-by-one | 3/3 · 75 ± 5 · 0.03 ± 0.00 · 2.0 ± 0.0 | 3/3 · 23 ± 3 · 0.01 ± 0.00 · 3.0 ± 1.0 |
| long-backlog | 3/3 · 150 ± 13 · 0.13 ± 0.01 · 5.0 ± 0.0 | 3/3 · 111 ± 9 · 0.02 ± 0.00 · 8.3 ± 1.2 |
| multi-repo-currency | 3/3 · 152 ± 26 · 0.07 ± 0.01 · 5.0 ± 0.0 | 3/3 · 103 ± 18 · 0.03 ± 0.00 · 13.3 ± 0.6 |
| multi-repo-pagination | 2/3 · 272 ± 60 · 0.12 ± 0.04 · 4.3 ± 0.6 | 2/3 · 86 ± 14 · 0.02 ± 0.00 · 13.3 ± 4.0 |
| quality-ledger-transfer | 3/3 · 153 ± 49 · 0.05 ± 0.01 · 2.0 ± 0.0 | 3/3 · 46 ± 8 · 0.01 ± 0.00 · 5.0 ± 0.0 |
| quality-retry-budget | 3/3 · 158 ± 61 · 0.04 ± 0.00 · 2.0 ± 0.0 | 3/3 · 45 ± 9 · 0.01 ± 0.00 · 5.7 ± 3.1 |
| quality-safe-extract | 3/3 · 334 ± 34 · 0.12 ± 0.01 · 2.0 ± 0.0 | 3/3 · 84 ± 14 · 0.02 ± 0.00 · 5.7 ± 1.5 |
| refactor-order-status | 3/3 · 264 ± 158 · 0.07 ± 0.04 · 3.3 ± 0.6 | 3/3 · 63 ± 14 · 0.02 ± 0.00 · 6.7 ± 1.5 |
| review-seeded-bug | 3/3 · 45 ± 5 · 0.01 ± 0.00 · 1.0 ± 0.0 | 3/3 · 43 ± 18 · 0.01 ± 0.00 · 2.7 ± 1.5 |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| adaptive | 4 | 4/4 (100%) | 308 ± 59 | 0.11 ± 0.01 | 0 | 0 |
| direct | 22 | 21/22 (95%) | 156 ± 61 | 0.06 ± 0.04 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 38 ± 9 | 0.01 ± 0.00 | 0 | 0 |
| refactor | 1 | 1/1 (100%) | 445 ± 0 | 0.11 ± 0.00 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| mixed: standard + strict | 2 | 1/2 (50%) | 293 ± 67 | 0.14 ± 0.04 | 0 | 0 |
| no work (empty queue) | 6 | 6/6 (100%) | 38 ± 9 | 0.01 ± 0.00 | 0 | 0 |
| standard | 21 | 21/21 (100%) | 146 ± 44 | 0.06 ± 0.03 | 0 | 0 |
| strict | 4 | 4/4 (100%) | 362 ± 62 | 0.12 ± 0.01 | 0 | 0 |

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
