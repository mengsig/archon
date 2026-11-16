# Release 1.6.0 smoke gate

**Verdict: PASS — no decrease observed after #423.** Haiku passed **8/8**
Archon trials per version; Luna passed **4/4** per version. Direct-Aether
controls passed **12/12** per version. Retry-budget, the previous blocker,
passed **3/3 on v1.5.1 and 3/3 on 1.6.0** in this fresh comparison.
There were no failed or timed-out trials and no one-sided failures to confirm.
This is a small smoke comparison, not a statistically powered release benchmark.

Archon 1.6.0 requires **Aether 1.4.0**. The candidate includes the
[intent-gate fix #423](https://github.com/mengsig/archon/pull/423), merged
from `origin/next` at `33f70e5c` into the release preparation.

Haiku's mean Archon task time per run was **661.0 s on v1.5.1 versus
523.2 s on 1.6.0 (−20.8%)**. Mean reported cost was **≥ $0.20967 versus
$0.18537**. The old mean is a lower bound, so an exact percentage cost
change cannot be claimed. Luna's totals were **776.1 s versus 580.8 s
(−25.2%)**, with **$0.07995 versus $0.06201** in incomplete estimates;
those costs are neither exact nor bounds. Review choices and repairs vary
substantially, so these timing differences are observations, not evidence
of a reliable speedup.

## Fresh runs after the fix

Execution order, one foreground run at a time. Wall and cost sum only
Archon task trials. Each run also has four direct-Aether controls.

| Run | Load5 | Archon | Direct Aether | Archon wall | Archon cost | Reviewers |
|---|---:|---:|---:|---:|---:|---:|
| old-haiku-1 | 13.87 | 4/4 | 4/4 | 525.8 s | $0.18329 | 6 |
| new-haiku-1 | 10.67 | 4/4 | 4/4 | 680.9 s | $0.23693 | 9 |
| old-haiku-2 | 8.99 | 4/4 | 4/4 | 796.1 s | ≥ $0.23605 | 8 |
| new-haiku-2 | 10.38 | 4/4 | 4/4 | 365.6 s | $0.13382 | 5 |
| old-luna-1 | 9.05 | 4/4 | 4/4 | 776.1 s | * $0.07995 | 7 |
| new-luna-1 | 9.57 | 4/4 | 4/4 | 580.8 s | * $0.06201 | 4 |

`≥` is a lower bound: old-haiku-2's ledger-transfer usage was interrupted.
`*` mixes estimates with missing usage and bounds nothing. Luna costs are
API-equivalent estimates of subscription usage, not billed dollars.

Ledger-transfer illustrates the noise: its Archon times were 265 s and
516 s on old Haiku, 471 s and 140 s on new Haiku. The first new run needed
a ship-record repair after review, while the second old run delivered two
strict-reviewed items. Old Luna needed a reviewer-result formatting repair.
The retained records show the individual trials and repairs rather than
hiding them in an average. Hidden graders passed all delivered tasks.

Every new-arm code item in the captured records has `intent: null` and
acceptance `not_required`. In particular, retry-budget no longer acquired
an invented requirement ledger or failed for an unavailable independent
vision model. No intent feature, review policy, task, prompt, budget or
timeout was disabled or changed to get this result.

## Candidate and verification

All six runs used the candidate application and harness built from
`ce516b2b62e5ebbe99572e19b3f79006c308df4d`. The subsequent release report and
image-guide correction are prose only; application code, dependencies,
configuration and the bench harness are unchanged.

`CARGO_BUILD_JOBS=3 make ci` passed on that commit, tree
`3f620e1b2dca05dbff02900b8ee816190f95e212`, inside the host CI slot,
starting at load5 **10.44**. Its 111 test summaries total **4371 passed,
0 failed, 10 ignored**. Format, shell/release-script checks, clippy and
rustdoc with warnings denied also passed. No test needed isolated reruns.

The release build passed with `cargo build --release --locked -p archon
-p archon-bench`, also inside the host slot with three jobs, after waiting
for load5 to fall below 14 (start **13.79**). All cargo commands used the
item's private cache; development/test debug symbols and incremental
compilation were disabled to limit disk use.

After the report and guide edit, the required documentation-link and
release-notice checks are recorded with the final head in the ship record.
The last full CI tree above is stated separately from that prose-only head.

## Reproduction and isolation

The fixed tasks, model identifiers, medium effort, $7 admission limit,
serial execution and per-task deadlines are unchanged from the first gate.
Both arms use the same candidate `archon-bench`, including the configuration
compatibility fix that omits `limits.clarification_timeout_minutes` for
v1.5.1. No benchmark or product code changed during this retry.

Old: published Archon **1.5.1** and Aether **1.3.1** Linux musl binaries.
New: candidate Archon **1.6.0** and published Aether **1.4.0** Linux musl.
All three downloaded archives passed their release's `SHA256SUMS`.

| Binary | SHA256 prefix |
|---|---|
| Candidate Archon | `ddf69fb4908f` |
| Shared candidate archon-bench | `9e4030c3d9cf` |
| Published Archon 1.5.1 | `d721d7a7c0b5` |
| Published Aether 1.3.1 | `ad05f728843e` |
| Published Aether 1.4.0 | `5f884dd88443` |

The retained wrapper commands, from the run workspace, were:

```sh
python3 -u smoke-run.py old haiku old-haiku-1
python3 -u smoke-run.py new haiku new-haiku-1
python3 -u smoke-run.py old haiku old-haiku-2
python3 -u smoke-run.py new haiku new-haiku-2
python3 -u smoke-run.py old luna old-luna-1
python3 -u smoke-run.py new luna new-luna-1
```

Each invokes `archon-bench smoke` with explicit private binary, socket and
result paths. Luna additionally selects `codex/gpt-6-luna` and
`bench/results-smoke/baseline-luna.json`; Haiku uses
`claude-code/claude-haiku-5-5` and the stored Haiku baseline.

Each run started below load5 20 in a fresh private sandbox with an explicit
clean environment, isolated HOME, config, data, temporary directories and
sockets. Engine credentials were copied mode 600, never printed, and
removed with the sandbox. Each private daemon was stopped through its own
socket and recorded PID; all six PIDs have exited. Installed daemons were
not touched. Wall time excludes setup and the quiet completion window.

The artifact **release-1.6.0-retry-evidence**, item
`qi_01M7GRZDD3Y0JMVXR41WTGC0VW`, retains run ledgers, grader results,
item histories, daemon databases and logs, checksum manifests, full binary
hashes, wrapper scripts and CI/build logs. No credential homes or binaries
are included. The earlier regression artifacts remain available for audit.

## Superseded gate: before the intent fix

The following is the original blocked gate. Its verdict and omitted Luna
runs describe that earlier attempt, not the successful retry above.

**DECREASE: blocked on intent-gate fix**

The initial Haiku smoke passed **8/8** Archon trials on v1.5.1 and **7/8** on 1.6.0. After two additional paired confirmations, retry-budget passed **4/4 on v1.5.1** and **2/4 on 1.6.0**, including the initial trials. The repeated failure is a product regression in the approved-intent gates from [#422](https://github.com/mengsig/archon/pull/422). Release preparation is retained, but the release must wait for the intent-gate fix and a fresh gate.

The two full-smoke runs averaged 408.1 s on v1.5.1 and 599.6 s on 1.6.0 (+46.9%). Reported mean costs were ≥ $0.12612 and $0.18996; the old cost is a lower bound, so their ratio is not a cost-regression estimate. Review choices vary on both versions. The release blocker is repeated failed delivery, not a statistical claim about these small timing or cost samples.

### Runs

In execution order; wall and cost columns sum Archon task trials only. Confirmations select ledger-transfer and retry-budget. The direct-Aether column is the corresponding control.

| Run | Load5 | Archon | Direct Aether | Archon wall | Archon cost | Reviewers |
|---|---:|---:|---:|---:|---:|---:|
| old-haiku-1 | 11.24 | 4/4 | 4/4 | 370.5 s | ≥ $0.12534 | 3 |
| new-haiku-1 | 9.07 | 4/4 | 4/4 | 396.8 s | $0.13328 | 3 |
| old-haiku-2 | 9.09 | 4/4 | 4/4 | 445.7 s | $0.12691 | 3 |
| new-haiku-2 | 13.95 | 3/4 | 4/4 | 802.5 s | $0.24663 | 10 |
| old-haiku-confirm-1 | 13.37 | 2/2 | 2/2 | 430.6 s | $0.11815 | 5 |
| new-haiku-confirm-1 | 12.68 | 1/2 | 2/2 | 376.7 s | ≥ $0.14091 | 4 |
| old-haiku-confirm-2 | 10.08 | 2/2 | 2/2 | 240.4 s | $0.09600 | 4 |
| new-haiku-confirm-2 | 10.57 | 2/2 | 2/2 | 380.6 s | $0.13482 | 6 |

### Failure and confirmation evidence

In `new-haiku-2`, retry-budget delivered the unchanged `NotImplementedError` stub, an empty patch and a failing hidden grader (14 tests, 48 errors). It was neither a timeout nor a harness error. Its item carried an approved intent with an empty requirements list. The first failed run reported `missing requirement_results`; the repair was refused because independent acceptance required a permitted, connected vision model different from every builder. The unchanged smoke policy permits one model.

The first new confirmation reproduced the same delivery failure with another empty-requirements intent. Its runs reported missing requirement results, an invalid `pass` requirement status, and the same unavailable independent vision model. Both passing new trials (the first smoke and second confirmation) had `intent: null`. The two targeted confirmations alone passed 2/2 retry-budget trials on v1.5.1 and 1/2 on 1.6.0. Direct-Aether controls passed all 12/12 trials per version across full smoke and confirmations. No intent gate was disabled and no model policy, task, grader, budget or timeout was changed to obtain a pass.

Retry-budget outcomes (initial runs followed by the two confirmations):

| Version | Smoke 1 | Smoke 2 | Confirmation 1 | Confirmation 2 |
|---|---|---|---|---|
| v1.5.1 | pass | pass | pass | pass |
| 1.6.0 | pass | FAIL | FAIL | pass |

Ledger-transfer passed throughout. Its slow new smoke trial took 316 s with strict review and a review-record repair, but the first old confirmation also took 310 s. That variation warrants caution about attributing aggregate cost and time to the release alone.

### Method and limits

Archon 1.6.0 requires **Aether 1.4.0**. The old arm uses the published
Archon 1.5.1 and Aether 1.3.1 Linux musl binaries. The new arm uses the
release-profile Archon 1.6.0 binary and published Aether 1.4.0 Linux musl
binary. All three published archives were checked against their release's
`SHA256SUMS`. A source build of Aether was stopped when 1.4.0 was published;
none of the measured runs uses that source build.

Both arms use one `archon-bench`, built from `9785576b` with the config
compatibility fix. Archon was built from `1b2751b6`; its application source
and dependencies are unchanged at `9785576b`. The only subsequent code
changes are in the bench harness and its tests. The report commit adds
only this evidence document.

The fixed four smoke tasks are `fix-off-by-one`, `quality-ledger-transfer`,
`quality-retry-budget` and `review-seeded-bug`. Each run has one trial per
task on both the Archon and direct-Aether arms, medium effort and the
unchanged $7 admission limit. The full Haiku runs alternate old 1, new 1, old 2, new 2. After the
one-sided failure, two more paired runs per version select only ledger and
retry-budget, still alternating old/new and one at a time. They confirm the
failed task and investigate the ledger cost outlier without changing its
settings. Every run starts below load5 20; the recorded loads are in the
results table. This is smoke evidence, not a statistically powered release
comparison. Targeted `run` commands render results without a smoke-baseline
exit-code gate; their pass/fail outcomes here come from the grader rows.

**Luna was not run.** The release rule stops this gate after the confirmed
intent regression. Independent release review was not requested while that
blocking failure remained. Merge the intent-gate fix, then rerun the gate,
including Luna, before requesting a passing release review.

Every run gets a fresh private sandbox outside the checkout, with its own
HOME, temporary directory, config, data and sockets. Subprocesses receive
an explicit clean environment. Engine credentials are copied with mode
600, never printed, and removed with the sandbox after the run. Each
private aetherd is stopped through its own socket and recorded PID. No
installed daemon is restarted. Results, grader output, logs and the
sandbox Aether database are retained as evidence; credential homes are not.

Wall time excludes daemon startup and the quiet window used to detect
completion. Costs include reported dollars plus estimates where present;
`≥` denotes incomplete reported usage and is not an exact cost. Individual
runs and per-task results are retained rather than treating repeated
passes as a reliability estimate.

### Verification

`CARGO_BUILD_JOBS=3 make ci` passed at `1b2751b6`, tree
`af0a997b6af4b9c3d9fad2bb9b74d4255784bcb5`, inside the host CI slot,
starting at load5 **13.62**. The 111 test-result lines total **4352 passed,
0 failed, 10 ignored**. Format, shell/release-script checks, clippy and
rustdoc with warnings denied passed. No test needed isolated reruns.
The initial dependency-resolution attempt caught accidental third-party
lockfile bumps; a separate commit restored them before the passing run.
All 168 third-party lock entries are unchanged from the release base.

After that full check, only `bench/src/arms/mod.rs`,
`bench/src/arms/tests.rs` and `CHANGELOG.md` changed. Per the amended release
instructions, full CI was not repeated for this bench-only fix. On
`9785576b`, at load5 **11.19**, the host-slot checks passed:

```sh
cargo fmt --all -- --check
cargo clippy -p archon-bench --all-targets --locked -- -D warnings
cargo test -p archon-bench --lib --bins --tests --locked
cargo test -p archon-bench --doc --locked
cargo build --release --locked -p archon-bench
```

The bench tests total **192 passed, 0 failed, 1 ignored**; doctests passed.
Cargo used `CARGO_BUILD_JOBS=3` and the item's private target cache.
Development/test debug symbols and incremental compilation were disabled
to limit disk use. `archon-bench verify` also passed. The final document passed the repository's documentation-link and
release-notice checks, inside the host CI slot at load5 10.53.

### Excluded setup failure

The first old-arm attempt did not start archond: the current harness wrote
`limits.clarification_timeout_minutes`, a new default that 1.5.1 refuses.
Its Archon cells failed before listening; its direct-Aether cells passed.
This was a harness compatibility failure, not a measured release decrease.
The attempt is retained separately as `invalid-old-haiku-config`.

The harness now writes only the sections it configures and checks that
reading the emitted TOML preserves the full current configuration. A
regression assertion excludes the incompatible default. The corrected
harness is used on both versions; prompts, tasks, graders, model settings,
budgets and timeouts are unchanged. Its first compile caught a redundant
`str::as_str` call, fixed before the passing targeted checks.

### Evidence collection note

After old-haiku-2 passed, copying its stopped daemon data encountered
transient Claude HTTP sockets. The result files and database were already
preserved; the database passed `PRAGMA integrity_check`, and the sandbox
was removed. Later copies exclude temporary socket directories. This
post-run collection error does not change any measured trial outcome.

### Reproduction and retained records

The retained `smoke-run.py` creates and removes each private sandbox and supplies explicit binary paths and a clean environment. Its commands were:

```sh
python3 -u smoke-run.py old haiku old-haiku-1
python3 -u smoke-run.py new haiku new-haiku-1
python3 -u smoke-run.py old haiku old-haiku-2
python3 -u smoke-run.py new haiku new-haiku-2
python3 -u smoke-run.py old haiku old-haiku-confirm-1 quality-ledger-transfer,quality-retry-budget
python3 -u smoke-run.py new haiku new-haiku-confirm-1 quality-ledger-transfer,quality-retry-budget
python3 -u smoke-run.py old haiku old-haiku-confirm-2 quality-ledger-transfer,quality-retry-budget
python3 -u smoke-run.py new haiku new-haiku-confirm-2 quality-ledger-transfer,quality-retry-budget
```

Full runs invoke `archon-bench smoke`; confirmations invoke `archon-bench run --arms archon,aether --only quality-ledger-transfer,quality-retry-budget --trials 1 --parallel 1 --effort medium --max-usd 7 --model claude-code/claude-haiku-5-5`. Each command also supplies its private `--archon-bin`, `--aether-socket` and `--results-dir`. The wrapper and exact binary hashes are preserved for reproduction.

Archon artifact store, item `qi_01M7GRZDD3Y0JMVXR41WTGC0VW`:

- `intent-gate-regression-evidence`: focused failed-item records, grader output and passing control for the fix item.
- `release-1.6.0-gate-evidence`: all run ledgers, item histories, grader output, daemon databases, scripts, provenance, checksum manifests and verification logs. No credential homes or binaries are included.

The final report passed `cargo test -p archon --test docs_links --test release_scripts --locked`. Full CI above is the last full check, not a claim that the blocked release passed its smoke gate.
