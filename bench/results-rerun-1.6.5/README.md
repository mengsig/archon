# Clean targeted Haiku rerun: Archon v1.6.5 vs v1.6.4

## Verdict

**No regression in pass rate. A cost increase on 1.6.5 is likely real, and a wall-time increase is not shown.**

Archon arm only, `claude-code/claude-haiku-5-5`, medium effort, Aether v1.4.3. One trial per private daemon, OLD and NEW interleaved. 84 graded trials (OLD 42, NEW 42).

| Set | OLD (1.6.4) pass | NEW (1.6.5) pass | Fisher p | Cost NEW/OLD [95% CI] | Wall NEW/OLD [95% CI] |
|---|---|---|---|---|---|
| A: 3 tasks that newly failed, 8 trials each | 21/24 | 24/24 | 0.23 | 1.52 [1.05, 2.16] | 1.18 [0.87, 1.58] |
| B: 6 other tasks, 3 trials each | 18/18 | 16/18 | 0.49 | 1.25 [0.67, 2.22] | 1.64 [0.74, 3.28] |
| All | 39/42 | 40/42 | 1.00 | 1.44 [1.01, 2.00] | 1.31 [0.90, 1.86] |

- Pass rate: the full-run drop (32/33 to 29/33) did not reproduce. In set A NEW passed 24/24 and OLD 21/24. The three set-A failures are all OLD `multi-repo-pagination`.
- Cost: NEW is about 1.4x OLD (CI just above 1). The excess is concentrated in `quality-safe-extract` (NEW mean $0.225 vs $0.109), `feature-token-bucket` and `refactor-order-status`. Totals are small: $7.77 spent in all, against the $25 guard.
- Wall: the 1.31 ratio is not significant, and the NEW mean includes one 1500 s timeout (`feature-token-bucket` t2).
- Mean per task, OLD/NEW: see `summary.json` (`per_task`).

## Failures (all 5)

| Task, trial | Arm | Kind | Cause (from transcript and grader output) |
|---|---|---|---|
| multi-repo-pagination t4 | OLD | wrong result | API accepts a 64-zero cursor; `test_bad_cursors` expects 400, got 200 |
| multi-repo-pagination t5 | OLD | wrong result | same `test_bad_cursors` failure (200 != 400) |
| multi-repo-pagination t7 | OLD | wrong result | same failure; the client branch also conflicted and was left out |
| feature-token-bucket t2 | NEW | timeout (1502 s) | the review round cap stopped the stage after 4 failed rounds on float-tolerance findings in `_has_tokens` and `wait_time`; the orchestrator re-queued and the bench timed out |
| ask-cache-invalidation t3 | NEW | wrong result | the final chat message was only "the report from my previous message stands unchanged", so the graded answer had no root cause (score 0.0) |

One further trial (`review-seeded-bug` t3 OLD) hit a harness error, "event 82 is not the orchestrator's". It is an infrastructure failure, kept in `infra/` and excluded. It was rerun and passed (`runs/review-seeded-bug-t3-old`).

## Likely cause of the extra cost

Not established. In `quality-safe-extract` the NEW transcripts show more reviewer-found blocking findings and fix rounds (for example failing reviews in t2, t4, t6 and t8, on non-encodable names, NotADirectoryError and lone surrogates). That fits `pipelines/direct/implement.md` and `pipelines/adaptive/build.md` changing the check wording (`pipelines/_shared/design-live.md` is new; `adaptive/build.md` adds the three-failed-rounds cap). It cannot be separated from the orchestrator choosing `direct` or `adaptive` per run. The `feature-token-bucket` timeout is the 4-round cap at work, but that cap is also the intended behaviour. No prompt was changed here and nothing was tuned to these tasks.

## Method and deviations

- Arms: OLD = published Archon v1.6.4, NEW = published v1.6.5 archives; both and Aether v1.4.3 x86_64-unknown-linux-musl verified with `sha256sum -c` against their `SHA256SUMS`. Binary hashes are in each `start.json`.
- Harness: each arm used the `archon-bench` built from its own release source (1.6.4 for OLD, 1.6.5 for NEW), as in the full run. The two harness binaries differ, so harness differences are confounded with arm.
- Schedule (`scripts/run.py`): set A 8 trials per (task, arm) for `multi-repo-pagination`, `quality-safe-extract`, `quality-retry-budget`; set B 3 trials for `ask-cache-invalidation`, `review-seeded-bug`, `fix-off-by-one`, `feature-token-bucket`, `refactor-order-status`, `long-backlog`. Order OLD, NEW within each pair, with pairs rotating through tasks.
- Relaxed load guard (deviation, operator-approved): the brief required load5 < 4 and no cargo, rustc, chromium or playwright process. The run held only on cargo, rustc and playwright processes, with load5 < 8, and ignored chromium (the long-lived design preview browser). load1 and load5 at each trial start are in `start.json`: load5 ranged 0.38 to 7.84. Other agents' cargo builds ran on the host during part of the run, hitting both arms alike by interleaving.
- The run was paused and restarted (operator was gaming, then a guard stall). One trial interrupted mid-run (`quality-safe-extract` t2 NEW) was discarded and rerun; the finished trials were kept.
- Pass rates exclude infra failures. Fisher exact is two-sided; ratios are ratios of means with a 4000-draw bootstrap (`scripts/analyze.py`, seed 7).
- Raw results: `runs/<task>-t<n>-<arm>/` (harness results, `archond.log`, `archon-items.json`, grader output). The Aether SQLite databases (about 220 MB) were left out; set A pass rates are small-n and p-values are not corrected for multiple looks.
