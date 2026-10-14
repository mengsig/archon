# Archon benchmark

- Run `20261014T071033Z+20261014T073720Z`: 2026-10-14T07:10:33.896245418+00:00 → 2026-10-14T07:54:53.573696618+00:00
- Archon commit: `9b5ddbc8e0eb37925582a6b765c3717269361a67`
- Effort: `high`; trials per task and arm: 2; parallel: 2
- Model `aether`: `claude-code/claude-opus-5-5`
- Model `archon`: `claude-code/claude-opus-5-5`
- Version `aetherd`: `1.0.3`
- Version `archon`: `archon 1.1.0`
- Trials: 26 planned, 26 completed, 0 skipped; spent $25.11 of $58.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-opus-5-5` | 13 | 13/13 (100%) | 100% | 100% | 1.00 ± 0.00 | 158 ± 119 | 1109611 ± 855682 | 18751 ± 15648 | 1.54 ± 1.16 | 3.1 ± 1.7 | 0 | 0 |
| aether | `claude-code/claude-opus-5-5` | 13 | 13/13 (100%) | 100% | 100% | 1.00 ± 0.00 | 74 ± 43 | 248929 ± 161467 | 7492 ± 4789 | 0.39 ± 0.18 | 2.8 ± 2.2 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 1/1 · 30 ± 0 · 0.39 ± 0.00 · 1.0 ± 0.0 | 1/1 · 35 ± 0 · 0.20 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 2/2 · 353 ± 180 · 1.78 ± 0.26 · 2.5 ± 0.7 | 2/2 · 73 ± 7 · 0.37 ± 0.02 · 2.5 ± 2.1 |
| fix-off-by-one | 2/2 · 50 ± 7 · 0.51 ± 0.03 · 2.0 ± 0.0 | 2/2 · 19 ± 4 · 0.16 ± 0.00 · 1.0 ± 0.0 |
| long-backlog | 2/2 · 210 ± 49 · 3.76 ± 0.96 · 6.0 ± 1.4 | 2/2 · 143 ± 35 · 0.66 ± 0.11 · 3.0 ± 1.4 |
| multi-repo-currency | 1/1 · 135 ± 0 · 1.63 ± 0.00 · 4.0 ± 0.0 | 1/1 · 98 ± 0 · 0.52 ± 0.00 · 8.0 ± 0.0 |
| multi-repo-pagination | 2/2 · 145 ± 21 · 1.56 ± 0.02 · 4.0 ± 0.0 | 2/2 · 103 ± 9 · 0.52 ± 0.06 · 5.0 ± 0.0 |
| refactor-order-status | 2/2 · 163 ± 25 · 1.22 ± 0.21 · 2.5 ± 0.7 | 2/2 · 61 ± 3 · 0.38 ± 0.01 · 2.0 ± 0.0 |
| review-seeded-bug | 1/1 · 45 ± 0 · 0.30 ± 0.00 · 1.0 ± 0.0 | 1/1 · 40 ± 0 · 0.22 ± 0.00 · 1.0 ± 0.0 |

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
