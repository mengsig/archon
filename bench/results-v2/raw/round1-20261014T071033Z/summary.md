# Archon benchmark

- Run `20261014T071033Z`: 2026-10-14T07:10:33.896245418+00:00 → 2026-10-14T07:37:03.984157903+00:00
- Archon commit: `9b5ddbc8e0eb37925582a6b765c3717269361a67`
- Effort: `high`; trials per task and arm: 1; parallel: 2
- Model `aether`: `claude-code/claude-opus-5-5`
- Model `archon`: `claude-code/claude-opus-5-5`
- Version `aetherd`: `1.0.3`
- Version `archon`: `archon 1.1.0`
- Trials: 16 planned, 16 completed, 0 skipped; spent $13.44 of $38.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-opus-5-5` | 8 | 8/8 (100%) | 100% | 100% | 1.00 ± 0.00 | 153 ± 144 | 918345 ± 706242 | 16867 ± 14807 | 1.32 ± 0.95 | 2.6 ± 1.5 | 0 | 0 |
| aether | `claude-code/claude-opus-5-5` | 8 | 8/8 (100%) | 100% | 100% | 1.00 ± 0.00 | 67 ± 36 | 217597 ± 128087 | 6822 ± 4367 | 0.36 ± 0.16 | 2.6 ± 2.6 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 1/1 · 30 ± 0 · 0.39 ± 0.00 · 1.0 ± 0.0 | 1/1 · 35 ± 0 · 0.20 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 1/1 · 480 ± 0 · 1.97 ± 0.00 · 2.0 ± 0.0 | 1/1 · 78 ± 0 · 0.39 ± 0.00 · 1.0 ± 0.0 |
| fix-off-by-one | 1/1 · 55 ± 0 · 0.54 ± 0.00 · 2.0 ± 0.0 | 1/1 · 16 ± 0 · 0.16 ± 0.00 · 1.0 ± 0.0 |
| long-backlog | 1/1 · 175 ± 0 · 3.08 ± 0.00 · 5.0 ± 0.0 | 1/1 · 119 ± 0 · 0.58 ± 0.00 · 2.0 ± 0.0 |
| multi-repo-currency | 1/1 · 135 ± 0 · 1.63 ± 0.00 · 4.0 ± 0.0 | 1/1 · 98 ± 0 · 0.52 ± 0.00 · 8.0 ± 0.0 |
| multi-repo-pagination | 1/1 · 160 ± 0 · 1.54 ± 0.00 · 4.0 ± 0.0 | 1/1 · 97 ± 0 · 0.48 ± 0.00 · 5.0 ± 0.0 |
| refactor-order-status | 1/1 · 145 ± 0 · 1.08 ± 0.00 · 2.0 ± 0.0 | 1/1 · 59 ± 0 · 0.37 ± 0.00 · 2.0 ± 0.0 |
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
