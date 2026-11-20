# Release 1.6.4 smoke gate

**Smoke verdict: PASS on correctness, with a time cost to note.** On the
release head (`bb7722dc`, on `next` at #456), Luna passed **4/4 on published
v1.6.3 and 4/4 on the candidate**. Haiku passed **4/4 on v1.6.3 and 3/4 on
the candidate**. The candidate's one miss is a harness refusal, not a wrong
answer (see below), and the task's confirmation runs miss at the same rate
on both arms. This is a small smoke, not a statistical pass.

**The candidate was slower in 3 of 4 pairs.** Luna's Archon time was +34%
and +14% in the two rounds run (pre-rebase and final head), and Haiku's was
-25% and +31% (+46% on the three tasks both graded). Most candidate stages
in the final round carry `verdict_note = missing_verdict_line`: their first
finish had no verdict line, so 1.6.4's one restatement turn (#450) ran. That
turn is intended, but on the `direct` pipeline it now fires on most stages,
which costs time on every item. It is reported here as a finding, not
tuned. Costs are incomplete or lower bounds, so their direction is unknown.

## Final head runs

One run at a time, alternating old and new, each in its own private
sandbox. A full run has four Archon tasks and four direct-Aether controls.
Time and cost sum only the Archon trials. `*` marks incomplete subscription
estimates, which are not billed dollars.

| Run | Load5 | Archon | Direct Aether | Archon time | Archon cost |
|---|---:|---:|---:|---:|---:|
| old-haiku-1 | 4.44 | 4/4 | 4/4 | 325.7 s | $0.1421 |
| new-haiku-1 | 1.80 | 3/4 | 4/4 | 425.7 s | $0.1605 |
| old-luna-1 | 0.90 | 4/4 | 4/4 | 661.1 s | * $0.0655 |
| new-luna-1 | 0.93 | 4/4 | 4/4 | 756.2 s | * $0.0693 |

| Archon task | Haiku old | Haiku new | Luna old | Luna new |
|---|---:|---:|---:|---:|
| fix-off-by-one | 60.3 s | 60.1 s | 195.3 s | 240.4 s |
| quality-ledger-transfer | 135.2 s | 215.3 s | 220.3 s | 260.4 s |
| quality-retry-budget | 95.2 s | 150.3 s | 190.3 s | 205.3 s |
| review-seeded-bug | 35.1 s | void | 55.1 s | 50.1 s |

**The candidate Haiku `review-seeded-bug` trial is void.** Answering the
review, the candidate's orchestrator delegated a helper agent. Aether lists
the helper's `agent.created` event among the orchestrator's events, because
the orchestrator created it. The harness (`bench/src/arms/archon/replies.rs`)
requires every listed event to be the orchestrator's own, so it stopped the
trial ("event 1729 is not the orchestrator's") with no answer graded. Only
the candidate arm met it here. The harness is unchanged since v1.6.3 apart
from one line, and it is left as it is in this release.

Confirmation, 2 more runs per arm of Haiku `review-seeded-bug`:

| Run | Load5 | Archon | Direct Aether |
|---|---:|---:|---:|
| old-haiku-rsb-2 | 0.88 | pass | pass |
| new-haiku-rsb-2 | 0.90 | pass | pass |
| old-haiku-rsb-3 | 1.11 | miss | pass |
| new-haiku-rsb-3 | 3.11 | miss | pass |

Both misses are the same grading miss: the review did not name `verify`'s
signature-prefix acceptance or tie it to forgery. Across all graded trials of
this task, v1.6.3 passed 2 of 3 and the candidate 1 of 2.

## Pre-rebase round

The first round ran on `ddaeb13e`, a candidate built before #455 was in the
branch. Archon passed 4/4 on every run. Haiku was 470.7 s (old) against
350.7 s (new); Luna was 530.9 s against 711.1 s. One Luna direct-Aether
control missed `review-seeded-bug`, and 2 more runs per arm of that task
passed all 8 trials. Its results are kept in the artifact under
`pre-rebase/`.

## Method

- The candidate is `archon` 1.6.4 built with `cargo build --release --locked
  -p archon -p archon-bench` at `bb7722dc`. The only later commit adds this
  file.
- The old arm is the published `archon` v1.6.3 (`x86_64-unknown-linux-musl`
  archive, passing its release's `SHA256SUMS`).
- Both arms use the published Aether v1.4.2 (`x86_64-unknown-linux-musl`,
  passing its release's `SHA256SUMS`).
- The old arm runs `archon-bench` built from the `v1.6.3` tag. The head's
  harness writes archond's config from the 1.6.4 `Config`, whose new
  `[orchestrator] compaction_tokens` key a 1.6.3 archond refuses, so a first
  old run on the head's harness failed at archond start and is not counted.
  `bench/tasks`, `bench/graders` and both smoke baselines are identical
  between `v1.6.3` and the head.
- Haiku uses `claude-code/claude-haiku-5-5`; Luna uses `codex/gpt-6-luna`
  and `bench/results-smoke/baseline-luna.json`. Medium effort, one trial per
  task, serial, the $7 admission limit.
- The wrapper is 1.6.2's `smoke-run.py`, with this release's paths and the
  per-arm `archon-bench`:

```sh
python3 -u .archon/reports/release-1.6.4/smoke-run.py old haiku old-haiku-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py new haiku new-haiku-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py old luna old-luna-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py new luna new-luna-1
python3 -u .archon/reports/release-1.6.4/smoke-run.py old haiku old-haiku-rsb-2 review-seeded-bug
# then new-haiku-rsb-2, old-haiku-rsb-3, new-haiku-rsb-3
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
| Candidate `archon` 1.6.4 | `6dad53a9e315ca0913f3f1ca825561dc113f25a6e6636fff9d39c0a455c8a580` |
| Candidate `archon-bench` | `540641ff8cf11d93d2104c52092e6aff3aa4b34350e25ed5aa41a38d9d4a7a7d` |
| `archon-bench` from `v1.6.3` | `a15dce0e58f1ab5cab2eed4cff2fc0fde598687a96513127a9e024f1bb8106ff` |

## Checks

`CARGO_BUILD_JOBS=3 make ci` passed on `14899dfd` (tree `959e636b`) inside
the host CI slot, started at load5 1.01: 5,149 tests passed, 0 failed, 10
ignored. Formatting, shell checks, clippy and rustdoc with warnings denied
passed. No test needed a rerun. The two commits after it change only
`CHANGELOG.md` and this file.

The artifact `release-1.6.4-evidence` of item `qi_01M7V498MT3J6GRE8EA7ZHAHZW`
keeps the results, item histories, private daemon logs, checksums, wrapper
and CI logs. Credentials and downloaded binaries are left out.
