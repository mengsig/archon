# Release 1.6.0 smoke gate

**DECREASE: blocked on intent-gate fix**

The initial Haiku smoke passed **8/8** Archon trials on v1.5.1 and **7/8** on 1.6.0. After two additional paired confirmations, retry-budget passed **4/4 on v1.5.1** and **2/4 on 1.6.0**, including the initial trials. The repeated failure is a product regression in the approved-intent gates from [#422](https://github.com/mengsig/archon/pull/422). Release preparation is retained, but the release must wait for the intent-gate fix and a fresh gate.

The two full-smoke runs averaged 408.1 s on v1.5.1 and 599.6 s on 1.6.0 (+46.9%). Reported mean costs were ≥ $0.12612 and $0.18996; the old cost is a lower bound, so their ratio is not a cost-regression estimate. Review choices vary on both versions. The release blocker is repeated failed delivery, not a statistical claim about these small timing or cost samples.

## Runs

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

## Failure and confirmation evidence

In `new-haiku-2`, retry-budget delivered the unchanged `NotImplementedError` stub, an empty patch and a failing hidden grader (14 tests, 48 errors). It was neither a timeout nor a harness error. Its item carried an approved intent with an empty requirements list. The first failed run reported `missing requirement_results`; the repair was refused because independent acceptance required a permitted, connected vision model different from every builder. The unchanged smoke policy permits one model.

The first new confirmation reproduced the same delivery failure with another empty-requirements intent. Its runs reported missing requirement results, an invalid `pass` requirement status, and the same unavailable independent vision model. Both passing new trials (the first smoke and second confirmation) had `intent: null`. The two targeted confirmations alone passed 2/2 retry-budget trials on v1.5.1 and 1/2 on 1.6.0. Direct-Aether controls passed all 12/12 trials per version across full smoke and confirmations. No intent gate was disabled and no model policy, task, grader, budget or timeout was changed to obtain a pass.

Retry-budget outcomes (initial runs followed by the two confirmations):

| Version | Smoke 1 | Smoke 2 | Confirmation 1 | Confirmation 2 |
|---|---|---|---|---|
| v1.5.1 | pass | pass | pass | pass |
| 1.6.0 | pass | FAIL | FAIL | pass |

Ledger-transfer passed throughout. Its slow new smoke trial took 316 s with strict review and a review-record repair, but the first old confirmation also took 310 s. That variation warrants caution about attributing aggregate cost and time to the release alone.

## Method and limits

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

## Verification

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

## Excluded setup failure

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

## Evidence collection note

After old-haiku-2 passed, copying its stopped daemon data encountered
transient Claude HTTP sockets. The result files and database were already
preserved; the database passed `PRAGMA integrity_check`, and the sandbox
was removed. Later copies exclude temporary socket directories. This
post-run collection error does not change any measured trial outcome.

## Reproduction and retained records

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
