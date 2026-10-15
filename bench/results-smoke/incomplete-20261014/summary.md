# Archon benchmark

- Run `20261014T121928Z`: 2026-10-14T12:19:28.012166533+00:00 → unfinished
- Archon commit: `3da3e125b810b8acf3076bbf57d536c174a01b07`
- Effort: `high`; trials per task and arm: 1; parallel: 1
- Model `aether`: `claude-code/claude-haiku-5-5`
- Model `archon`: `claude-code/claude-haiku-5-5`
- Version `aetherd`: `unreachable: io: connecting to aetherd at $XDG_DATA_HOME/archon-bench/qg7h/run/aetherd.sock: No such file or directory (os error 2)`
- Version `archon`: `archon 1.1.0`
- Trials: 8 planned, 0 completed, 0 skipped; spent $0.00 of $0.70

## Per arm

| Arm | Model | Trials | Success | pass@1 | pass@1 | Score | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra | Timeouts |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | `claude-code/claude-haiku-5-5` | 2 | 2/2 (100%) | 100% | 100% | 1.00 ± 0.00 | 208 ± 167 | 1696867 ± 1352227 | 41234 ± 43783 | 0.08 ± 0.06 | 2.0 ± 0.0 | 0 | 0 |
| aether | `claude-code/claude-haiku-5-5` | 1 | 1/1 (100%) | 100% | 100% | 1.00 ± 0.00 | 35 ± 0 | 199635 ± 0 | 3272 ± 0 | 0.01 ± 0.00 | 4.0 ± 0.0 | 0 | 0 |

## Per task

Each cell: passes/trials · wall s · cost $ · operator msgs (mean ± stdev).

| Task | archon | aether |
|---|---|---|
| fix-off-by-one | 1/1 · 90 ± 0 · 0.03 ± 0.00 · 2.0 ± 0.0 | 1/1 · 35 ± 0 · 0.01 ± 0.00 · 4.0 ± 0.0 |
| quality-ledger-transfer | 1/1 · 326 ± 0 · 0.12 ± 0.00 · 2.0 ± 0.0 | — |

## Per pipeline

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| direct | 2 | 2/2 (100%) | 208 ± 167 | 0.08 ± 0.06 | 0 | 0 |

## Per review level

Archon whole-trial observations: each trial appears once per table. Mixed buckets retain all selected and attempted pipelines or selected review levels. Pass rate is the trial grader verdict; cost and wall time cover the whole trial, including orchestration. These are not per-item outcomes or causal comparisons. Review levels are captured policy, not proof of completed review; historical policy changes are unavailable. Unknown means missing metadata; no work means a captured empty queue. Infrastructure failures are excluded from metrics.

| Bucket | Trials | Pass rate | Wall s | Cost $ | Infra | Timeouts |
|---|---:|---:|---:|---:|---:|---:|
| standard | 1 | 1/1 (100%) | 90 ± 0 | 0.03 ± 0.00 | 0 | 0 |
| strict | 1 | 1/1 (100%) | 326 ± 0 | 0.12 ± 0.00 | 0 | 0 |

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
