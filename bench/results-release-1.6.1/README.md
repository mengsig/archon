# Release 1.6.1 smoke gate

**Smoke verdict: PASS — no confirmed release regression.** Haiku passed
**4/4 per version**. Luna initially passed **3/4 on v1.6.0 and 4/4 on
1.6.1**; the one-sided review-task failure passed **2/2 additional trials
per version**. Including those confirmations, Luna passed **5/6 old versus
6/6 new**. Direct-Aether controls had the same pass counts. No trial timed
out or had an infrastructure error. This is a small smoke comparison, not
a statistical pass or evidence of a reliable improvement.

**Performance is inconclusive, with slower candidate observations.** Haiku
Archon task time rose from **425.6 s to 710.9 s (+67.0%)**, and reported
cost from **$0.14305 to $0.19899 (+39.1%)**. Luna task time rose from
**485.6 s to 567.1 s (+16.8%)**; incomplete cost estimates were
**$0.05947 versus $0.06179**. These single runs do not establish that the
slowdown is repeatable or rule out a performance regression. The Haiku
ledger trial accounts for 271 s of the 285 s difference: its candidate
needed extra review and repair, described below. No confirmed decrease
in delivered correctness was observed.

## Release scope

Archon 1.6.1 requires **Aether 1.4.0**. Its pin is unchanged. Since v1.6.0:

- [#426](https://github.com/mengsig/archon/pull/426) records the Luna
  post-release comparison of 1.5.1 against 1.5.0.
- [#428](https://github.com/mengsig/archon/pull/428) validates intent drafts
  before accepting stage results, including the `behavior` alias for
  `behaviour`.
- [#429](https://github.com/mengsig/archon/pull/429) caps attached references
  at 16, including at most 8 images, and lists overflow references by path.

Preparation updates workspace versions, current documentation and the
changelog. The web redesign belongs to 1.6.2. The orchestrator corrected
the brief to preserve the web app's independent `APP_VERSION = '1.0.0'`,
which #424 also preserved (clarification `ntf_01M7M8SZ7665Z49BDYJ8P66H8K`);
the recorded timeout fallback likewise preserves existing behavior.

## Runs

Execution order, one foreground run at a time. Wall and cost sum only
Archon trials; each full run also includes four direct-Aether controls.
Confirmation runs select only `review-seeded-bug` and its control.

| Run | Load5 | Archon | Direct Aether | Archon wall | Archon cost | Reviewers |
|---|---:|---:|---:|---:|---:|---:|
| old-haiku-1 | 6.17 | 4/4 | 4/4 | 425.6 s | $0.14305 | 4 |
| new-haiku-1 | 2.54 | 4/4 | 4/4 | 710.9 s | $0.19899 | 7 |
| old-luna-1 | 2.22 | 3/4 | 3/4 | 485.6 s | * $0.05947 | 3 |
| new-luna-1 | 1.95 | 4/4 | 4/4 | 567.1 s | * $0.06179 | 3 |
| old-luna-confirm-1 | 4.30 | 1/1 | 1/1 | 30.1 s | $0.00263 | 0 |
| new-luna-confirm-1 | 3.43 | 1/1 | 1/1 | 40.1 s | $0.00674 | 0 |
| old-luna-confirm-2 | 2.93 | 1/1 | 1/1 | 35.1 s | $0.00421 | 0 |
| new-luna-confirm-2 | 2.66 | 1/1 | 1/1 | 25.1 s | $0.00344 | 0 |

`*` combines estimates with missing usage and is neither an exact total
nor a bound. Luna costs are API-equivalent subscription estimates, not
billed dollars. Timing excludes setup and the quiet completion window.

## Failures and variability

Old Luna's initial review task scored 0.5 in both arms. Its Archon answer
identified `expected.startsWith(signature)` and truncated signatures, but
omitted the empty-signature forgery required by the fixed rubric. The
candidate initially scored 1.0 in both arms. Two additional alternating
runs per version all scored 1.0. The original failures remain in the
records: old review success is 2/3, candidate 3/3. The old full smoke
therefore exited 1 against the stored 4/4 baseline; the candidate full
smoke exited 0. Targeted confirmations use `run`, so their outcomes come
from the grader records, not a smoke-baseline exit code.

Haiku ledger-transfer passed in 195.2 s old versus 466.0 s new. Candidate
review found concurrency tests that did not detect removal of the lock;
the worker added lock-presence tests and obtained delta review. The
candidate run used seven reviewers overall versus four old. This is
observable extra work, not evidence that #428 or #429 caused a slowdown.
Luna ledger-transfer went the other direction (210.3 s old versus
171.6 s new), while its off-by-one and retry-budget trials were slower.
No tasks, prompts, models, budgets, graders, review policies or timeouts
were changed to improve outcomes. Confirmation covered the one-sided
failure, not timing; a powered performance comparison was not run.

## Build and CI

Both candidate binaries and every smoke run used commit
`934f49a42cd14073997abf2c6b0a1d4741080e8a`, tree
`9f308555e1541ba408821b978a9af5615b5a688b`. The subsequent report commit
adds only this prose file, with no application or harness changes.

`CARGO_BUILD_JOBS=3 make ci` passed on that tree inside the host CI slot,
starting at load5 **3.54**: **4,380 passed, 0 failed, 10 ignored** across
111 summaries. Formatting, shell checks, clippy and rustdoc with warnings
denied passed. No flaky test needed reruns. The final ship record supplies
the repeated full-CI result and tree after adding this report.

`cargo build --release --locked -p archon -p archon-bench` passed inside
the host slot at load5 **6.99**, with three jobs. All Cargo commands used
the item's private target cache. Development/test debug symbols and
incremental compilation were disabled to limit disk usage.

## Reproduction and isolation

Both versions use the same candidate `archon-bench` and the published
Aether **1.4.0** binary. The old Archon binary is the published **1.6.0**
Linux musl release. Both downloaded archives matched their release's
`SHA256SUMS`:

| Archive | SHA256 |
|---|---|
| Archon 1.6.0 Linux x86_64 musl | `e03a64de6e67a9e48281f631b8cc2daeafaf57762a0a79ee7a3f2d7eef80a5f5` |
| Aether 1.4.0 Linux x86_64 musl | `2b5c51430d17390adbebc37173f2b611778b9da36082968b8c58117c69a03a83` |

| Binary | SHA256 |
|---|---|
| Candidate Archon | `db9fc55dae32efd1bd1a7ea2775931721b0e5d453f6f9b49328fea07f4c8005f` |
| Shared candidate archon-bench | `ac809e24e2263c6f9b87dd90fb4e4ccdc74c25862bef72ab4d31515576d6565b` |
| Published Archon 1.6.0 | `0f25bd522093ab6bbea52cb27a4cd27b8ecbdff121e0ad73d2a3b293723beaba` |
| Published Aether 1.4.0 | `5f884dd88443ca5fbd2937a85be1114e97ed029bb56cdc01f87ab5d1c62dfeaf` |

The retained wrapper was adapted from the 1.6.0 release artifact. From the
run workspace, it was invoked serially as follows:

```sh
python3 -u .archon/reports/release-1.6.1/smoke-run.py old haiku old-haiku-1
python3 -u .archon/reports/release-1.6.1/smoke-run.py new haiku new-haiku-1
python3 -u .archon/reports/release-1.6.1/smoke-run.py old luna old-luna-1
python3 -u .archon/reports/release-1.6.1/smoke-run.py new luna new-luna-1
python3 -u .archon/reports/release-1.6.1/smoke-run.py old luna old-luna-confirm-1 review-seeded-bug
python3 -u .archon/reports/release-1.6.1/smoke-run.py new luna new-luna-confirm-1 review-seeded-bug
python3 -u .archon/reports/release-1.6.1/smoke-run.py old luna old-luna-confirm-2 review-seeded-bug
python3 -u .archon/reports/release-1.6.1/smoke-run.py new luna new-luna-confirm-2 review-seeded-bug
```

Full runs invoke `archon-bench smoke` with private binary, socket and
result paths. Luna selects `codex/gpt-6-luna` and
`bench/results-smoke/baseline-luna.json`; Haiku uses
`claude-code/claude-haiku-5-5` and the stored Haiku baseline. Both use
medium effort, one trial per task, serial execution and the unchanged
$7 admission limit. The four fixed tasks are `fix-off-by-one`,
`quality-ledger-transfer`, `quality-retry-budget` and `review-seeded-bug`.

Each run started below load5 20 in a fresh private sandbox, with isolated
HOME, config, data, temporary paths and sockets. Engine credentials were
copied mode 600, never printed, and deleted with the sandbox. Each daemon
was stopped through its private socket and recorded PID. All eight Aether
PIDs have exited; the harness completed its Archon teardown. Installed
daemons and their config/data were not changed.

Artifact **release-1.6.1-evidence**, item
`qi_01M7M8QGSZZ2SQX5QWHRFCJ71T`, retains run results, grader output, item
histories, private daemon logs/databases, checksum manifests, wrapper and
CI/build logs. Credential homes and downloaded binaries are excluded.
