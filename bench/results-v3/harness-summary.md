# Archon benchmark

- Run `20261014T091017Z+20261014T095439Z`: 2026-10-14T09:10:17.727739577+00:00 → 2026-10-14T10:18:19.227198115+00:00
- Archon commit: `1298fb064a6c8c0c9853e4d5aed4cefa7509a1c3+0f61cb74a0cdcde74a7ff72c891d69761ff05dc1`
- Effort: `high`; trials per task and arm: 2; parallel: 2
- Model `aether`: `claude-code/claude-opus-5-5`
- Model `archon`: `claude-code/claude-opus-5-5`
- Version `aetherd`: `1.0.3`
- Version `archon`: `archon 1.1.0`
- Trials: 32 planned, 32 completed, 0 skipped; spent $25.25 of $46.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@2 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-opus-5-5` | 16 | 14/16 (88%) | 88% | 100% | 0.92 ± 0.25 | 156 ± 158 | 797844 ± 507743 | 13981 ± 10276 | 1.19 ± 0.75 | 2.7 ± 1.5 | 0 | 0 |
| aether | `claude-code/claude-opus-5-5` | 16 | 15/16 (94%) | 94% | 100% | 0.94 ± 0.25 | 77 ± 43 | 246575 ± 161269 | 7297 ± 4744 | 0.39 ± 0.17 | 2.9 ± 2.2 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 2/2 · 33 ± 4 · 0.43 ± 0.00 · 1.0 ± 0.0 | 2/2 · 33 ± 6 · 0.22 ± 0.02 · 1.0 ± 0.0 |
| feature-token-bucket | 2/2 · 510 ± 191 · 1.83 ± 0.56 · 2.0 ± 0.0 | 2/2 · 89 ± 36 · 0.43 ± 0.12 · 3.0 ± 1.4 |
| fix-off-by-one | 2/2 · 38 ± 4 · 0.42 ± 0.00 · 2.0 ± 0.0 | 2/2 · 24 ± 6 · 0.17 ± 0.00 · 1.0 ± 0.0 |
| long-backlog | 1/2 · 158 ± 32 · 2.29 ± 0.07 · 5.0 ± 0.0 | 2/2 · 154 ± 8 · 0.71 ± 0.05 · 5.5 ± 2.1 |
| multi-repo-currency | 1/2 · 146 ± 6 · 1.71 ± 0.06 · 4.5 ± 0.7 | 2/2 · 100 ± 9 · 0.49 ± 0.01 · 5.5 ± 2.1 |
| multi-repo-pagination | 2/2 · 186 ± 41 · 1.62 ± 0.01 · 4.0 ± 0.0 | 2/2 · 101 ± 24 · 0.46 ± 0.01 · 5.0 ± 1.4 |
| refactor-order-status | 2/2 · 126 ± 6 · 0.93 ± 0.01 · 2.0 ± 0.0 | 1/2 · 67 ± 16 · 0.36 ± 0.03 · 1.5 ± 0.7 |
| review-seeded-bug | 2/2 · 50 ± 0 · 0.31 ± 0.01 · 1.0 ± 0.0 | 2/2 · 49 ± 1 · 0.25 ± 0.01 · 1.0 ± 0.0 |

## Method

- Every arm gets the same task prompt, effort and time limit, and the same model where the harness allows; `codex` runs its own model. Every arm runs with its tool permission prompts bypassed.
- `claude` keeps Claude Code's built-in subagents (the Task tool); the `aether` arm is a single leaf agent without delegation.
- Each trial starts from a fresh copy of the fixtures in an unguessably named directory outside the Archon checkout, with a one-commit git history. Hidden tests, reference solutions and rubrics are copied in only at grade time, into a separate copy the agent never sees, after the agent and every process it started have stopped. Agent processes do not inherit environment variables pointing at the checkout. There is no OS sandbox: an agent that searched the whole home directory could still find the checkout.
- Success is the hidden grader's verdict. Test graders restore the visible tests from the task, not from the agent's repository, and require the runner's own summary to report no failures and a minimum number of passing tests. Rubric graders need related facts close together and cap the answer length.
- A failed trial with a harness error (the arm could not run cleanly: a provider limit, a crash, a grading failure) is an infrastructure failure. It is left out of pass rates and every metric, counted under Infra and listed with its error.
- Wall time runs from handing over the prompt until the arm finished; Archon's daemon start-up (recorded as `setup_secs`) and the quiet window used to detect that Archon finished are excluded.
- Input tokens include cache reads and writes. Cost is billed dollars when reported, else Aether's estimate or `--price` list prices. `(n=…)` marks a metric only that many trials reported.
- Operator messages are what the operator had to read: one final result for `claude`/`codex`, the assistant messages of the Aether agent, and the default-visible chat lines for Archon.
- Values are mean ± sample standard deviation over graded trials. pass@k uses the unbiased estimator per task, averaged over tasks, with k the fewest graded trials any (task, arm) cell has.
