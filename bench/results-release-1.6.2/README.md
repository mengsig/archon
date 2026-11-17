# Release 1.6.2 smoke gate

**Smoke verdict: PASS — no confirmed release regression.** Haiku and Luna
both passed **4/4 on published v1.6.1 and 4/4 on the 1.6.2 candidate**.
Direct-Aether controls also passed 4/4 in every run. All 32 trials completed
without a timeout or infrastructure error. There were no one-sided failures
to confirm. This is a small correctness smoke, not a statistical pass or
proof that performance is unchanged.

**Performance is inconclusive, with slower candidate observations.** Haiku
Archon task time rose from **427.6 s to 440.7 s (+3.1%)**, and reported cost
from **$0.12363 to $0.13713 (+10.9%)**. Luna task time rose from **485.6 s to
545.8 s (+12.4%)**; incomplete API-equivalent subscription cost estimates
were **$0.04473 versus $0.06027**. Those incomplete estimates are neither
exact totals nor bounds, and are not billed dollars. These single runs
cannot establish repeatability or rule out a performance regression.

## Release scope

Archon 1.6.2 requires **Aether 1.4.0**. Its pin is unchanged.
`git log v1.6.1..origin/next` contains only
`6f4fc57acd025ea3741853ed6612a6e79d401850`,
[PR #433](https://github.com/mengsig/archon/pull/433), implementing the
[operator's web app request #427](https://github.com/mengsig/archon/issues/427):

- Redesigned phone and desktop workspace with dependency, work and agent graphs.
- Observer devices see no operator controls; a change of session device or
  role remounts the app.
- Fewer connections per tab, a visible "Can't connect" state and bounded,
  jittered scheduled retries that honor a busy host's `Retry-After`.
- Additive `onto` task-graph edges, sent only to clients declaring the
  capability, preserving compatibility with older protocol-24 clients.

Release preparation finishes the current-version documentation, dates the
single 1.6.2 changelog section 2026-11-18 (Asia/Dubai) and leaves Unreleased
empty. Cargo.toml, Cargo.lock, README.md and the web app's `APP_VERSION`
already said 1.6.2 in #433 and are preserved. The retry note now says
"scheduled retries": page-show and pairing rechecks can read a session
before a pending `Retry-After`. This correction changes no retry code.
Install scripts already select Aether 1.4.0 and need no change.

## Runs

One foreground run at a time, alternating old and new. Each full run
contains four Archon tasks and four direct-Aether controls. Time and cost
below sum only Archon trials; time excludes setup and the quiet completion
window.

| Run | Load5 | Archon | Direct Aether | Archon time | Archon cost |
|---|---:|---:|---:|---:|---:|
| old-haiku-1 | 5.12 | 4/4 | 4/4 | 427.6 s | $0.12363 |
| new-haiku-1 | 3.31 | 4/4 | 4/4 | 440.7 s | $0.13713 |
| old-luna-1 | 3.08 | 4/4 | 4/4 | 485.6 s | * $0.04473 |
| new-luna-1 | 2.77 | 4/4 | 4/4 | 545.8 s | * $0.06027 |

`*` marks incomplete estimates. Old Luna has usage gaps on off-by-one and
retry-budget; candidate Luna also has a gap on ledger-transfer.

| Archon task | Haiku old | Haiku new | Luna old | Luna new |
|---|---:|---:|---:|---:|
| fix-off-by-one | 85.1 s | 80.1 s | 150.2 s | 130.2 s |
| quality-ledger-transfer | 172.2 s | 195.3 s | 105.1 s | 185.3 s |
| quality-retry-budget | 130.2 s | 130.2 s | 200.2 s | 195.3 s |
| review-seeded-bug | 40.1 s | 35.1 s | 30.1 s | 35.1 s |

Luna's ledger task accounts for 80.2 s of the net 60.2 s difference. The
saved item histories show the old orchestrator chose `review: none` for
that task; the candidate chose `review: standard` and delegated one
reviewer. That is observable extra work, not evidence that the release
caused the slowdown. No task, prompt, model, grader, budget, timeout or
benchmark review setting was changed to improve outcomes. No powered
performance comparison was run.

## Build and checks

Candidate binaries, all four smoke runs and the web checks used commit
`bd7dcdc0c592a2808e774d4bebf97915279a0ec5`, tree
`a67b5a7ee5b8367c91dad12dc19852e96efb98e9`. The subsequent report commit
adds only this prose file; application and harness inputs are unchanged.
The final ship record supplies the full-CI result and tree after adding
this report.

`CARGO_BUILD_JOBS=3 make ci` passed inside the host CI slot at load5
**2.85**: **4,413 passed, 0 failed, 10 ignored** across 111 summaries.
Formatting, shell checks, clippy and rustdoc with warnings denied passed.
No flaky test needed a rerun. Initial CI inherited the agent runtime's
temporary directory; subsequent checks use `/tmp` and explicit private
data/config paths and sockets.

`cargo build --release --locked -p archon -p archon-bench` passed in
**6m 27s**, inside the host slot at load5 **8.58**, with three jobs. Every
Cargo command used this item's private target cache; development/test
debug symbols and incremental compilation were disabled to limit disk use.

The existing `node crates/archon-remote/web/scripts/e2e.js` harness passed
against the candidate release binary, starting at load5 **5.93**:
**176 unit tests and 108 Chromium tests passed**. The two intentional skips
are desktop-only graph-fit cases on phone widths, where readable scale and
panning are expected. Browser installation was not skipped. The flows cover
both themes, phone and desktop layouts, graphs, observer/operator controls,
reconnect recovery and device revocation.

## Reproduction and isolation

Both versions use the same candidate `archon-bench` and published Aether
**1.4.0**. The old Archon is the published **1.6.1 Linux x86_64 musl**
release. Both downloaded archives matched their release's `SHA256SUMS`:

| Archive | SHA256 |
|---|---|
| Archon 1.6.1 Linux x86_64 musl | `d1a6cf627f7369cec4061058daa8a2a727ab4ef29541526b3e78091e11e20fc6` |
| Aether 1.4.0 Linux x86_64 musl | `2b5c51430d17390adbebc37173f2b611778b9da36082968b8c58117c69a03a83` |

| Binary | SHA256 |
|---|---|
| Candidate Archon | `a13dcc993767f8f4af9914e11e163337e6c050dc469d275fda097ec0fc02d744` |
| Shared candidate archon-bench | `ba4423ffbfc5b0357f2b3e79ce9717ab548d343b2e7676e3bb11780c2bcdd27f` |
| Published Archon 1.6.1 | `4dc90e6806665d6a23055ca822a7ba22415391b4b0547ea6b0e97fc15e0d42ff` |
| Published Aether 1.4.0 | `5f884dd88443ca5fbd2937a85be1114e97ed029bb56cdc01f87ab5d1c62dfeaf` |

The wrapper was adapted from the retained 1.6.1 release artifact, changing
only release/item paths. From this workspace it was invoked serially:

```sh
python3 -u .archon/reports/release-1.6.2/smoke-run.py old haiku old-haiku-1
python3 -u .archon/reports/release-1.6.2/smoke-run.py new haiku new-haiku-1
python3 -u .archon/reports/release-1.6.2/smoke-run.py old luna old-luna-1
python3 -u .archon/reports/release-1.6.2/smoke-run.py new luna new-luna-1
```

Each invokes `archon-bench smoke` with private binary, socket and result
paths. Luna selects `codex/gpt-6-luna` and
`bench/results-smoke/baseline-luna.json`; Haiku uses
`claude-code/claude-haiku-5-5` and the stored Haiku baseline. Both use
medium effort, one trial per task, serial execution and the unchanged
$7 admission limit.

Each run started below load5 20 in a fresh private sandbox, with isolated
HOME, config, data, temporary paths and sockets. Engine credentials were
copied mode 600, never printed, and deleted with the sandbox. Daemons were
stopped through private sockets and recorded PIDs; all four Aether PIDs
have exited and all four sandbox roots were verified removed. The harness
completed its Archon teardown. Installed daemons and their configuration
were not modified.

Artifact **release-1.6.2-evidence**, item
`qi_01M7NQJ8TKMP1813P3JNQYEEF4`, retains grader results, task histories,
private daemon logs/databases, checksums, wrappers and CI/build/web logs.
Credential homes and downloaded binaries are excluded.

Review: not run in stage; orchestrator runs a separate review item, as
instructed after withdrawing the original stage review requirement.
No tag, push, merge, GitHub post or release publication was performed here.
