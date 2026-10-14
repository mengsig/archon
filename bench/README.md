# archon-bench

`archon-bench` runs the same hidden-graded coding tasks through four ways of
getting work done and compares them on correctness, time, tokens, cost and how
much the operator has to read. The design is in
[`docs/ARCHITECTURE.md` §15](../docs/ARCHITECTURE.md#15-benchmarks); this file
explains the method and publishes the v1 results.

## Tasks

Eight tasks under [`tasks/`](tasks). Each has fixture repositories, a prompt,
a time limit, and a grader the agent never sees.

| Task | What it tests | Grader | Limit |
|---|---|---|---:|
| `fix-off-by-one` | Fix a bug shown by a failing test, correctly for every input and not just the tested ones (Python). | Hidden edge-case tests plus the visible ones. | 15 min |
| `feature-token-bucket` | Implement a rate limiter from a written spec, including error and edge cases the visible tests leave out (Python). | Hidden spec tests. | 25 min |
| `refactor-order-status` | Replace a stringly-typed status with an enum across a Rust crate, with no change in behaviour. | Hidden API and behaviour tests; status literals may appear only in `status.rs`. | 25 min |
| `multi-repo-pagination` | Add cursor pagination to a Python API and to its Node client, in two repositories. | Hidden tests in both repositories. | 30 min |
| `multi-repo-currency` | Thread a currency column through a Rust exporter and a Python report, including exit codes and backwards compatibility. | Hidden tests in both repositories. | 35 min |
| `long-backlog` | Work through four independent tickets in one repository, one commit each (Python). | Hidden acceptance tests for every ticket. | 60 min |
| `ask-cache-invalidation` | Answer a debugging question about stale prices without changing code (Node). | Keyword rubric on the answer. | 15 min |
| `review-seeded-bug` | Review an HMAC token module and find its seeded forgery bug (Node). | Keyword rubric on the answer. | 15 min |

`archon-bench verify` checks that every grader passes the reference solution
and fails the untouched fixture.

## Arms

| Arm | What runs |
|---|---|
| `archon` | A private `archond` per trial, with the fixture registered. The prompt is sent as an operator message, and Archon's orchestrator picks the pipeline, plans work items and runs its stage agents (implement, review and so on). |
| `aether` | One Aether leaf agent (`create_agent`) with the prompt, without delegation. |
| `claude` | `claude -p … --dangerously-skip-permissions`. Claude Code keeps its built-in Task subagents. |
| `codex` | `codex exec --dangerously-bypass-approvals-and-sandbox` on codex's own default model. |

Every arm gets the same prompt, effort and time limit, with tool permission
prompts bypassed. `archon`, `aether` and `claude` use the same model.
`archon` and `aether` run on the same Aether daemon.

## Isolation and anti-gaming

- Each trial starts from a fresh copy of the fixtures, `git init`ed with one
  commit tagged `bench-base`. The copy lives in an unguessably named directory
  under `~/.local/share/archon-bench/work`, outside the checkout, inside a
  traverse-only run directory.
- Hidden tests, reference solutions and rubrics are copied in only at grade
  time, into a separate copy. By then the agent and every process it started
  have stopped.
- Agent processes lose every environment variable and `PATH` entry that points
  into the checkout, the task suite or the results. `GIT_CEILING_DIRECTORIES`
  stops git from walking up out of the workspace.
- Test graders restore the visible tests from the task, not from the agent's
  repository. They require the runner's own summary to show no failures and a
  minimum number of passing tests, so deleted, skipped or disabled tests fail.
- Rubric graders need related facts within a window of each other and cap the
  answer length, so keyword dumps fail.

## Grading and statistics

- **Success** is the hidden grader's verdict. For `aether`, `claude` and
  `codex` it grades the workspace, including uncommitted changes. For
  `archon` it grades each work item's run branch merged onto `bench-base`.
- **Infrastructure failures** are trials that failed with a harness error: a
  provider limit, a crash or a grading failure. They say nothing about the
  model or the arm, so they are left out of every number and listed on their
  own.
- **Wall time** runs from handing over the prompt until the arm finished.
  Archon's daemon start-up and the quiet window that detects it has finished
  are excluded.
- **Tokens** are input (including cache reads and writes) and output tokens
  over every agent of the trial. For `archon` that includes the orchestrator
  and all stage agents.
- **Cost** is billed dollars when the harness reports them. Otherwise it is
  Aether's estimate, or list prices given with `--price`.
- **Operator messages** count what the operator has to read:
  - one final result for `claude` and `codex`;
  - the assistant messages of the `aether` agent;
  - the default-visible chat lines for `archon`.
- Every value is the mean ± sample standard deviation over graded trials.
- **pass@k** uses the unbiased estimator 1 − C(n−c, k)/C(n, k) per task,
  averaged over tasks. k is the fewest graded trials any (task, arm) cell has.

## Results (v1, 2026-09-26)

Snapshot: [`results-v1/`](results-v1). It holds
[`summary.md`](results-v1/summary.md), [`report.html`](results-v1/report.html),
the chart data and the run metadata; raw logs are not committed.

- Models: `claude-code/claude-opus-5-5` for `archon`, `aether` and `claude`;
  `codex/gpt-5.6-sol` (codex's default) for `codex`. Effort `high`.
- Versions: aetherd 0.1.1, Claude Code 2.1.283, codex-cli 0.155.1, and a
  pre-release build of archon, made before 1.0.0 and before dedicated mode.
  That build's commit is not in the public history, so the snapshot names
  neither it nor a version; every measurement is as the run recorded it.
- Planned: 8 tasks × 4 arms × 3 trials = 96. The `--max-usd` budget guard
  stopped the run after 52 trials (see [Missing cells](#missing-cells)).

| Arm | Graded trials | Success | Wall s | Input tok | Output tok | Cost $ | Operator msgs | Infra |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| archon | 12 | 12/12 (100%) | 279 ± 199 | 1.35M ± 0.94M | 29.2k ± 22.4k | 2.02 ± 1.39 | 3.7 ± 1.6 | 1 |
| aether | 13 | 13/13 (100%) | 91 ± 49 | 275k ± 165k | 8.4k ± 5.7k | 0.42 ± 0.20 | 3.5 ± 2.4 | 0 |
| claude | 12 | 12/12 (100%) | 77 ± 50 | 231k ± 164k | 7.5k ± 5.4k | 0.38 ± 0.19 | 1.0 ± 0.0 | 1 |
| codex | 13 | 13/13 (100%) | 215 ± 183 | 286k ± 196k | 8.3k ± 5.4k | 0.38 ± 0.22 ¹ | 1.0 ± 0.0 | 0 |

¹ Estimated from OpenAI's list price for gpt-5.6-sol ($4 input, $0.40 cached
input, $20 output per million tokens); codex does not report a cost.

k is 1 because some cells have only one graded trial, so pass@1 equals the
success rate: 100% for every arm.

Per task, each cell shows passes/graded trials · wall s · cost $:

| Task | archon | aether | claude | codex |
|---|---|---|---|---|
| ask-cache-invalidation | 2/2 · 35 · 0.23 | 2/2 · 34 · 0.20 | 2/2 · 32 · 0.20 | 2/2 · 63 · 0.11 |
| feature-token-bucket | 2/2 · 586 · 3.04 | 2/2 · 109 · 0.51 | 2/2 · 116 · 0.49 | 2/2 · 194 · 0.37 |
| fix-off-by-one | 2/2 · 182 · 0.64 | 2/2 · 45 · 0.16 | 2/2 · 18 · 0.16 | 2/2 · 57 · 0.17 |
| long-backlog | 2/2 · 390 · 3.97 | 2/2 · 165 · 0.72 | 1/1 · 154 · 0.71 | 2/2 · 563 · 0.75 |
| multi-repo-currency | 2/2 · 193 · 2.19 | 2/2 · 123 · 0.56 | 2/2 · 110 · 0.52 | 2/2 · 239 · 0.52 |
| multi-repo-pagination | 1/1 · 388 · 2.73 | 1/1 · 111 · 0.49 | 1/1 · 125 · 0.56 | 1/1 · 222 · 0.48 |
| refactor-order-status | 1/1 · 190 · 1.36 | 1/1 · 64 · 0.38 | 1/1 · 55 · 0.34 | 1/1 · 197 · 0.42 |
| review-seeded-bug | — (infra) | 1/1 · 55 · 0.27 | 1/1 · 41 · 0.22 | 1/1 · 140 · 0.23 |

Standard deviations and operator messages per cell are in
[`results-v1/summary.md`](results-v1/summary.md).

### Reading the results

- **Correctness:** no arm failed a graded trial. At this sample size the suite
  does not separate the arms on correctness; every task is within reach of a
  single strong agent.
- **Cost and time:** Archon costs about 5× a single agent and takes about 3×
  the wall time. It runs a full pipeline per work item: an orchestrator,
  implementation, then review and fix stages. The gap is widest on
  `feature-token-bucket`, where the orchestrator enqueued a follow-up item,
  and on `long-backlog`, where it ran four items in parallel, each through a
  review.
- **Operator attention:** Archon's operator reads about 4 chat lines per task:
  plan, progress and result. Plain Aether sends 3–4 assistant messages.
  `claude` and `codex` return one final message.

### Missing cells

The budget guard (`--max-usd 37`) is checked before each trial starts, so
trials already in flight overshot it: the run spent $40.01. Two pilot runs on
`fix-off-by-one` spent another ~$2.03, which puts the total at ~$42.04,
slightly over the planned $40.

The run is trial-major, so it stopped in the middle of round 2. Not run:

- trial 2 of `multi-repo-pagination`, `refactor-order-status` and
  `review-seeded-bug`, for every arm;
- all of trial 3.

That leaves n=2 for the first five tasks in the table and n=1 for the last
three.

### Infrastructure failures

Two trials are left out of every number:

- `review-seeded-bug` / archon #1: a harness bug. The `ask` pipeline succeeds
  without a branch, and grading tried to merge the empty branch name. It is
  fixed now (see below). An offline rubric regrade of the saved answer passes (score
  1.0), but it is not counted.
- `long-backlog` / claude #2: the Claude subscription hit its session limit
  during the run ("You've hit your session limit").

The same session limit also paused the Archon orchestrator in
`long-backlog` / archon #2. The supervisor woke it and the trial passed, but
its wall time includes about 60 s of waiting.

### Caveats

- **Small n.** One or two graded trials per cell cannot show differences in
  reliability; treat the success rates as "all arms can solve these tasks".
- **Estimated codex cost.** Codex reports tokens but not dollars. Its cost
  uses the list price above; the other arms report billed or Aether-estimated
  cost.
- **`claude` is not a single agent.** Claude Code may spawn Task subagents;
  their tokens and cost are in its totals. The `aether` arm is a single leaf
  agent.
- **No OS sandbox.** Workspaces are outside the checkout and hidden files
  arrive only after the agent stopped, but an agent that searched the whole
  home directory could still find the checkout. No trial log mentions the
  checkout, the task suite or a hidden file.
- **Host state.** The agents ran with the operator's own `~/.claude` and
  `~/.codex` setup (plugins, instructions), in a clean `env -i` environment.
- **Archon build.** The archon binary was built before dedicated Aether
  became the default. The bench pins Archon to the shared mode
  (`mode = Shared`), so `archon` and `aether` still use the same daemon.
- **Harness fixes.** Each fix came with a test.
  - The pilots found two bugs, fixed before the main run:
    - the aether arm read its events before its final turn ended;
    - archon teardown cancelled the orchestrator before `archond` exited, so
      the supervisor restarted it.
  - The main run found a third: branchless `ask` runs broke archon grading.
    It is fixed, but that trial stays an infrastructure failure.
  - After the run, the summary gained the separation of infrastructure
    failures. The published summary was re-rendered from the same
    `results.jsonl`.

## Later results

These snapshots rerun only the `archon` and `aether` arms on the same model.
Each has a `summary.md` with Archon's overhead per task.

- [`results-v2/`](results-v2/summary.md) covers Archon 1.1.0 as first built.
  Cost overhead was 3.69× and wall-time overhead 1.98×.
- [`results-v3/`](results-v3/summary.md) is Archon 1.1.0 after the cost work.
  Cost overhead fell to 3.09×, input-token overhead to 3.24×, and wall-time
  overhead stayed at 2.02×. `long-backlog` reached wall parity.

## Reproduce

Point the harness at a sandbox Aether daemon, never the one you work with
(see [Live tests](../CONTRIBUTING.md#live-tests)), whose environment has the
same `ARCHON_SOCKET`. Put the sandbox's `archon` binary,
built from this checkout, first on `PATH`. Then run:

```sh
archon-bench verify
archon-bench run --dry-run
archon-bench run --trials 3 --parallel 3 --effort high \
  --model claude-code/claude-opus-5-5 \
  --price gpt-5.6-sol=4,0.4,20 \
  --aether-socket ~/.local/share/archon-live/aether.sock \
  --max-usd 37
archon-bench report bench/results/<stamp>
```

(`archon-bench` is `cargo run -p archon-bench --`.) Results land in
`bench/results/<stamp>/`, which is gitignored.
