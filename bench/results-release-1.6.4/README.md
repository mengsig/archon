# Release 1.6.4 smoke gate

**Smoke verdict: PASS, no decrease in correctness.** Haiku and Luna both
passed **4/4 on published v1.6.3 and 4/4 on the 1.6.4 candidate**. No trial
timed out or hit an infrastructure error. This is a small smoke of one run
per model and arm, not a statistical pass.

**Time and cost moved in opposite directions, within what one run can show.**
Haiku's Archon task time fell from **470.7 s to 350.7 s (-25%)** and its cost
from **$0.1856 to ≥ $0.1408**. Luna's rose from **530.9 s to 711.1 s
(+34%)**, with incomplete cost estimates of **$0.0585 against $0.0778**.
Most of Luna's extra time is two pieces of work 1.6.4 does on purpose (see
below), and no task lost a grade.

One direct-Aether control missed once: `review-seeded-bug` on the candidate's
Luna run. That arm runs the same Aether 1.4.2 binary in both versions and no
Archon code. Two more runs per arm of that task passed every Archon and
Aether trial (8/8).

## Runs

One run at a time, alternating old and new, each in its own private sandbox.
A full run has four Archon tasks and four direct-Aether controls. Time and
cost sum only the Archon trials. `*` marks incomplete subscription estimates,
which are not billed dollars; `≥` marks a total with a usage gap.

| Run | Load5 | Archon | Direct Aether | Archon time | Archon cost |
|---|---:|---:|---:|---:|---:|
| old-haiku-1 | 3.26 | 4/4 | 4/4 | 470.7 s | $0.1856 |
| new-haiku-1 | 0.82 | 4/4 | 4/4 | 350.7 s | ≥ $0.1408 |
| old-luna-1 | 0.74 | 4/4 | 4/4 | 530.9 s | * $0.0585 |
| new-luna-1 | 0.79 | 4/4 | 3/4 | 711.1 s | * $0.0778 |

| Archon task | Haiku old | Haiku new | Luna old | Luna new |
|---|---:|---:|---:|---:|
| fix-off-by-one | 65.1 s | 65.1 s | 160.3 s | 120.2 s |
| quality-ledger-transfer | 250.4 s | 115.2 s | 160.3 s | 290.4 s |
| quality-retry-budget | 120.2 s | 145.3 s | 185.3 s | 260.4 s |
| review-seeded-bug | 35.1 s | 25.1 s | 25.1 s | 40.1 s |

Confirmation of the one-sided control miss, `review-seeded-bug` on Luna:

| Run | Load5 | Archon | Direct Aether |
|---|---:|---:|---:|
| old-luna-rsb-2 | 0.74 | pass | pass |
| new-luna-rsb-2 | 0.81 | pass | pass |
| old-luna-rsb-3 | 0.83 | pass | pass |
| new-luna-rsb-3 | 0.78 | pass | pass |

## What the Luna time is

The stored item histories show where the candidate spent its extra time:

- `quality-retry-budget` (+75 s): the first finish had no verdict line
  (`verdict_note = missing_verdict_line`). 1.6.4 asks such a stage once to
  restate its verdict (#450), and the restatement said `VERDICT: PASS`.
  v1.6.3 accepted the first finish as it was.
- `quality-ledger-transfer` (+130 s): the reviewer found a missing snapshot
  test. The fix was reviewed again and passed. v1.6.3's review passed the
  first round.

Both are review work, not a slower path. One run cannot show whether either
recurs at this rate. Haiku's faster ledger task is the mirror case: v1.6.3
chose `strict` review with four delegates, the candidate `standard` with one.
No task, prompt, model, grader, budget or timeout was changed.

## Method

- The candidate is `archon` 1.6.4 built with `cargo build --release --locked
  -p archon -p archon-bench` from the release commit `ddaeb13e`. The commit
  after it changes only `CHANGELOG.md`.
- The old arm is the published `archon` v1.6.3 (`x86_64-unknown-linux-musl`
  archive, passing its release's `SHA256SUMS`).
- Both arms use the published Aether v1.4.2 (`x86_64-unknown-linux-musl`,
  passing its release's `SHA256SUMS`).
- The old arm runs `archon-bench` built from the `v1.6.3` tag. The head's
  harness writes archond's config from the 1.6.4 `Config`, whose new
  `[orchestrator] compaction_tokens` key a 1.6.3 archond refuses, so a first
  old Haiku run on the head's harness failed every Archon trial at archond
  start. That run is not counted. `bench/tasks`, `bench/graders` and both
  smoke baselines are identical between `v1.6.3` and the head.
- Haiku uses `claude-code/claude-haiku-5-5` and the stored Haiku baseline.
  Luna uses `codex/gpt-6-luna` and `bench/results-smoke/baseline-luna.json`.
  Medium effort, one trial per task, serial, the $7 admission limit.
- The wrapper is 1.6.2's (`smoke-run.py` in that release's artifact), with
  this release's paths and the per-arm `archon-bench`:

```sh
python3 -u .archon/reports/release-1.6.4/smoke-run.py old haiku old-haiku-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py new haiku new-haiku-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py old luna old-luna-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py new luna new-luna-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py old luna old-luna-rsb-2 review-seeded-bug
# then new-luna-rsb-2, old-luna-rsb-3, new-luna-rsb-3
```

Each run started below load5 20, with HOME, config, data, temporary paths
and sockets of its own. Engine credentials were copied mode 600, never
printed, and removed with the sandbox. Each private aetherd was stopped
through its own socket or recorded PID. Every sandbox root was checked
removed afterwards. The installed daemons and their configuration were not
touched.

| Archive or binary | SHA256 |
|---|---|
| Archon 1.6.3 musl archive | `431df6836330da6a0abc4ba9ecfb1064bebfdc8be8a6b402ac5d33ccb7dd193d` |
| Aether 1.4.2 musl archive | `5fccab8c4606a09f33e9a4cfb8ee698976d84a8348101e7d6fd786d8d23cac34` |
| Published `archon` 1.6.3 | `8fe4927e3bc2d0c66e590e9fab58de8aca468cdfdc1345b07d3903da62b94981` |
| Published `aether` 1.4.2 | `c0510fe9d0669cf8eda7cc3f3154db21eb62c297b24b30896244e65398ab9a88` |
| Candidate `archon` 1.6.4 | `60f6712094c9f6db5976bdb29c1bafaddc262b1093549a7ae563200572ec0b82` |
| Candidate `archon-bench` | `540641ff8cf11d93d2104c52092e6aff3aa4b34350e25ed5aa41a38d9d4a7a7d` |
| `archon-bench` from `v1.6.3` | `a15dce0e58f1ab5cab2eed4cff2fc0fde598687a96513127a9e024f1bb8106ff` |

## Checks

`CARGO_BUILD_JOBS=3 make ci` passed on `ddaeb13e` (tree `f92fbf1b`) inside
the host CI slot, started at load5 2.53: 5,139 tests passed, 0 failed, 10
ignored. Formatting, shell checks, clippy and rustdoc with warnings denied
passed. No test needed a rerun.

The artifact `release-1.6.4-evidence` of item `qi_01M7V498MT3J6GRE8EA7ZHAHZW`
keeps the results, item histories, private daemon logs, checksums, wrapper
and CI log. Credentials and downloaded binaries are left out.
