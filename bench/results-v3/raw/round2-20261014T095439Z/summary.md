# Archon benchmark

- Run `20261014T095439Z`: 2026-10-14T09:54:39.675834773+00:00 → 2026-10-14T10:18:19.227198115+00:00
- Archon commit: `27ad3ed7cfb8cc526db241b6268c671ea9a3edb1`
- Effort: `high`; trials per task and arm: 1; parallel: 2
- Model `aether`: `claude-code/claude-opus-5-5`
- Model `archon`: `claude-code/claude-opus-5-5`
- Version `aetherd`: `1.0.3`
- Version `archon`: `archon 1.1.0`
- Trials: 16 planned, 16 completed, 0 skipped; spent $12.26 of $16.00

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-opus-5-5` | 8 | 7/8 (88%) | 88% | 88% | 0.88 ± 0.35 | 132 ± 111 | 761881 ± 474763 | 13282 ± 9594 | 1.13 ± 0.72 | 2.8 ± 1.7 | 0 | 0 |
| aether | `claude-code/claude-opus-5-5` | 8 | 7/8 (88%) | 88% | 88% | 0.88 ± 0.35 | 78 ± 46 | 260023 ± 190632 | 7654 ± 5144 | 0.40 ± 0.19 | 3.2 ± 2.4 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| ask-cache-invalidation | 1/1 · 30 ± 0 · 0.42 ± 0.00 · 1.0 ± 0.0 | 1/1 · 29 ± 0 · 0.20 ± 0.00 · 1.0 ± 0.0 |
| feature-token-bucket | 1/1 · 375 ± 0 · 1.43 ± 0.00 · 2.0 ± 0.0 | 1/1 · 115 ± 0 · 0.51 ± 0.00 · 4.0 ± 0.0 |
| fix-off-by-one | 1/1 · 35 ± 0 · 0.42 ± 0.00 · 2.0 ± 0.0 | 1/1 · 20 ± 0 · 0.16 ± 0.00 · 1.0 ± 0.0 |
| long-backlog | 1/1 · 135 ± 0 · 2.25 ± 0.00 · 5.0 ± 0.0 | 1/1 · 160 ± 0 · 0.75 ± 0.00 · 7.0 ± 0.0 |
| multi-repo-currency | 0/1 · 141 ± 0 · 1.67 ± 0.00 · 5.0 ± 0.0 | 1/1 · 94 ± 0 · 0.48 ± 0.00 · 4.0 ± 0.0 |
| multi-repo-pagination | 1/1 · 157 ± 0 · 1.63 ± 0.00 · 4.0 ± 0.0 | 1/1 · 84 ± 0 · 0.46 ± 0.00 · 6.0 ± 0.0 |
| refactor-order-status | 1/1 · 130 ± 0 · 0.94 ± 0.00 · 2.0 ± 0.0 | 0/1 · 78 ± 0 · 0.38 ± 0.00 · 2.0 ± 0.0 |
| review-seeded-bug | 1/1 · 50 ± 0 · 0.30 ± 0.00 · 1.0 ± 0.0 | 1/1 · 48 ± 0 · 0.25 ± 0.00 · 1.0 ± 0.0 |

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
