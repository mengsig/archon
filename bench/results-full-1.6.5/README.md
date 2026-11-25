# Archon 1.6.5 full benchmark

**NO REGRESSION**

Haiku Archon: OLD 32/33 versus NEW 29/33, McNemar p=0.25. Recorded cost rose 1.43× [0.97, 2.06] and wall time rose 1.49×; these are possible regressions that do not meet the significance criteria and are confounded by overlapping e2e validation. Cost gaps also leave actual cost uncertain.

NEW Haiku Archon failures: multi-repo-pagination trials 1 and 2 returned wrong results; quality-safe-extract trial 1 and quality-retry-budget trial 3 timed out. Both timed-out trials passed post-timeout grading but count as failures.


[Visual report](index.html) · [Numeric summary](summary.json)

## Haiku (claude-code/claude-haiku-5-5)

Cost gaps: NEW Archon: {'interrupted': 3}. Some recorded totals are floors, so actual cost differences remain uncertain.

Significant pass-rate drop: no; significant recorded-cost increase: no; significant slowdown: no.

| Version | Arm | Pass / graded | Pass % [95% CI] | Wall mean | Cost / trial | Reviewers / item | Infra |
|---|---|---:|---|---:|---:|---:|---:|
| OLD | Archon | 32/33 | 97.0% [84.7%, 99.5%] | 162 s | $0.0622 | 1.50 | 0 |
| OLD | Direct Aether | 32/33 | 97.0% [84.7%, 99.5%] | 62 s | $0.0148 | 0.00 | 0 |
| NEW | Archon | 29/33 | 87.9% [72.7%, 95.2%] | 241 s | $0.0889 | 1.73 | 0 |
| NEW | Direct Aether | 33/33 | 100.0% [89.6%, 100.0%] | 63 s | $0.0145 | 0.00 | 0 |

### NEW versus OLD

| Arm | Pass difference [95% CI] | OLD-only / NEW-only | McNemar p | Wall ratio [95% CI] (p) | Cost ratio [95% CI] (p) |
|---|---|---|---:|---|---|
| Archon | -9.1 pp [-24.5, +5.2] | 3 / 0 of 33 | 0.250 | 1.49× [1.03, 2.10] (0.160) | 1.43× [0.97, 2.06] (0.230) |
| Direct Aether | +3.0 pp [-7.7, +15.3] | 0 / 1 of 33 | 1.000 | 1.01× [0.80, 1.28] (0.974) | 0.98× [0.78, 1.23] (0.903) |

### Archon per task

| Task | OLD pass | NEW pass | OLD wall | NEW wall |
|---|---:|---:|---:|---:|
| ask-cache-invalidation | 3/3 | 3/3 | 30 s | 32 s |
| feature-token-bucket | 3/3 | 3/3 | 145 s | 234 s |
| fix-off-by-one | 3/3 | 3/3 | 75 s | 85 s |
| long-backlog | 3/3 | 3/3 | 150 s | 204 s |
| multi-repo-currency | 3/3 | 3/3 | 152 s | 202 s |
| multi-repo-pagination | 2/3 | 1/3 | 272 s | 270 s |
| quality-ledger-transfer | 3/3 | 3/3 | 153 s | 392 s |
| quality-retry-budget | 3/3 | 2/3 | 158 s | 329 s |
| quality-safe-extract | 3/3 | 2/3 | 334 s | 483 s |
| refactor-order-status | 3/3 | 3/3 | 264 s | 369 s |
| review-seeded-bug | 3/3 | 3/3 | 45 s | 50 s |

### Paired flips, slower tasks and infrastructure

- Archon, multi-repo-pagination: 2/3 to 1/3; 1 OLD-only and 0 NEW-only passes.
- Archon, quality-retry-budget: 3/3 to 2/3; 1 OLD-only and 0 NEW-only passes.
  - NEW trial 3: stage implement {'state': 'cancelled'}
- Archon, quality-safe-extract: 3/3 to 2/3; 1 OLD-only and 0 NEW-only passes.
  - NEW trial 1: stage implement {'state': 'cancelled'}
- Direct Aether, multi-repo-pagination: 2/3 to 3/3; 0 OLD-only and 1 NEW-only passes.
- Archon, feature-token-bucket: mean wall 145 s to 234 s (1.61×, p=0.827).
  - Trial 1: 160 s to 410 s.
- Archon, quality-ledger-transfer: mean wall 153 s to 392 s (2.56×, p=0.127).
  - Trial 1: 130 s to 625 s.
  - Trial 3: 210 s to 421 s.
- Archon, quality-retry-budget: mean wall 158 s to 329 s (2.09×, p=0.513).
  - Trial 3: 228 s to 722 s.
- Archon, quality-safe-extract: mean wall 334 s to 483 s (1.45×, p=0.275).
  - Trial 1: 345 s to 721 s.
- Archon, refactor-order-status: mean wall 264 s to 369 s (1.40×, p=0.513).
  - Trial 1: 165 s to 388 s.

## Luna (codex/gpt-6-luna)

Cost gaps: OLD Archon: {'mixed': 23}, NEW Archon: {'mixed': 22}. Some recorded totals are floors, so actual cost differences remain uncertain.

Significant pass-rate drop: no; significant recorded-cost increase: no; significant slowdown: no.

| Version | Arm | Pass / graded | Pass % [95% CI] | Wall mean | Cost / trial | Reviewers / item | Infra |
|---|---|---:|---|---:|---:|---:|---:|
| OLD | Archon | 29/33 | 87.9% [72.7%, 95.2%] | 240 s | $0.0281 | 1.19 | 0 |
| OLD | Direct Aether | 30/33 | 90.9% [76.4%, 96.9%] | 63 s | $0.0041 | 0.00 | 0 |
| NEW | Archon | 30/33 | 90.9% [76.4%, 96.9%] | 248 s | $0.0276 | 1.44 | 0 |
| NEW | Direct Aether | 30/33 | 90.9% [76.4%, 96.9%] | 65 s | $0.0043 | 0.00 | 0 |

### NEW versus OLD

| Arm | Pass difference [95% CI] | OLD-only / NEW-only | McNemar p | Wall ratio [95% CI] (p) | Cost ratio [95% CI] (p) |
|---|---|---|---:|---|---|
| Archon | +3.0 pp [-13.2, +19.4] | 3 / 4 of 33 | 1.000 | 1.04× [0.73, 1.47] (0.560) | 0.98× [0.68, 1.40] (0.631) |
| Direct Aether | +0.0 pp [-15.7, +15.7] | 2 / 2 of 33 | 1.000 | 1.02× [0.80, 1.30] (0.923) | 1.04× [0.82, 1.32] (0.649) |

### Archon per task

| Task | OLD pass | NEW pass | OLD wall | NEW wall |
|---|---:|---:|---:|---:|
| ask-cache-invalidation | 3/3 | 3/3 | 40 s | 40 s |
| feature-token-bucket | 3/3 | 3/3 | 290 s | 197 s |
| fix-off-by-one | 3/3 | 2/3 | 154 s | 122 s |
| long-backlog | 1/3 | 3/3 | 375 s | 559 s |
| multi-repo-currency | 2/3 | 2/3 | 217 s | 328 s |
| multi-repo-pagination | 3/3 | 3/3 | 392 s | 292 s |
| quality-ledger-transfer | 3/3 | 3/3 | 314 s | 442 s |
| quality-retry-budget | 2/3 | 3/3 | 167 s | 212 s |
| quality-safe-extract | 3/3 | 2/3 | 277 s | 239 s |
| refactor-order-status | 3/3 | 3/3 | 373 s | 259 s |
| review-seeded-bug | 3/3 | 3/3 | 37 s | 40 s |

### Paired flips, slower tasks and infrastructure

- Archon, fix-off-by-one: 3/3 to 2/3; 1 OLD-only and 0 NEW-only passes.
- Archon, long-backlog: 1/3 to 3/3; 0 OLD-only and 2 NEW-only passes.
  - OLD trial 2: long-backlog: branches ["archon/2vp35917-tt-103-search-query-syntax"] conflicted and were left out
  - OLD trial 3: long-backlog: branches ["archon/r0mbhemk-tt-103-search-query-syntax"] conflicted and were left out
- Archon, multi-repo-currency: 2/3 to 2/3; 1 OLD-only and 1 NEW-only passes.
- Archon, quality-retry-budget: 2/3 to 3/3; 0 OLD-only and 1 NEW-only passes.
- Archon, quality-safe-extract: 3/3 to 2/3; 1 OLD-only and 0 NEW-only passes.
- Direct Aether, ask-cache-invalidation: 2/3 to 3/3; 0 OLD-only and 1 NEW-only passes.
- Direct Aether, multi-repo-currency: 1/3 to 1/3; 1 OLD-only and 1 NEW-only passes.
- Direct Aether, quality-safe-extract: 3/3 to 2/3; 1 OLD-only and 0 NEW-only passes.
- Archon, long-backlog: mean wall 375 s to 559 s (1.49×, p=0.275).
  - Trial 3: 285 s to 911 s.
- Archon, multi-repo-currency: mean wall 217 s to 328 s (1.51×, p=0.275).
- Archon, quality-ledger-transfer: mean wall 314 s to 442 s (1.41×, p=0.827).
  - Trial 2: 225 s to 891 s.
- Archon, quality-retry-budget: mean wall 167 s to 212 s (1.26×, p=0.275).
  - Trial 1: 105 s to 235 s.

## Combined model spend

- Luna: $2.1270 including probes and all private daemon usage.
- Haiku: $7.1786 including probes and all private daemon usage.
## Method and provenance

Each run uses every task in `bench/tasks`, three trials per task and arm, medium effort, and Archon and direct-Aether arms. Design sessions remain at their default. Each run uses a private Aether daemon and isolated workspaces; Luna runs used `--parallel 1`. Haiku's initial serial runs were stopped on operator direction and restarted together at `--parallel 3`; the earlier cells are excluded from the final comparison but retained under `runs/probes/`.

An independent e2e validation began overlapping the Haiku runs by 08:32 UTC. Haiku wall-time and timeout comparisons are confounded by this concurrent workload; timeout-related pass/fail outcomes may also be affected. The runs continued without a restart, and observed load is recorded in the guard log. Interpret pass/fail and recorded cost alongside this limitation, and report any measured slowdown rather than attributing it solely to the candidate.

OLD is published Archon v1.6.4. NEW is built with `cargo build --release --locked -p archon -p archon-bench` at `c4bcaa9f8eff80ca81917f9ddaf65a866abb3690`. The `archon_commit` in a run's metadata is the reporting checkout's HEAD at run time, which can include later report-only commits; the candidate binary is the recorded build at `c4bcaa9f`. Both use published Aether v1.4.3. The x86_64 musl archives passed their release SHA256SUMS checks. OLD uses the v1.6.4-tag harness because the candidate harness speaks a newer incompatible Archon protocol; NEW uses the candidate harness. The candidate harness also includes the helper-event handling fix from #459, so observed differences may include harness effects. Task definitions are identical across those versions.

| Binary | SHA-256 |
|---|---|
| Archon v1.6.4 | `c82005af40e2aed8fe2dae7d10d96d29b38228d5af2268c5f671ef965cf65f5c` |
| Candidate Archon | `472d8f96480b524cd2d7b535ebc368325ae03ea9e8e40f3915c8fa49c4020253` |
| Aether v1.4.3 | `3a795ff2a718e8c227ac66ad6bbefd2cfe1838bfcbf77b37eac10e56ca3cc923` |
| v1.6.4 harness | `ef5186389d442b5e913b056a31e285d79c9321b25d5b54325caecc507b596632` |
| Candidate harness | `6d5650cdfd790bc18c2cef9563c42c2cda919f21c54ad568c0e28b90764dd0f8` |

The Archon and Aether release archive SHA-256 values are `12a79a01b3f2747a5918db45b4868e4a324a27733127bdff0468eae49476b091` and `25cf7df3db41fdab660fc5a5781e82f0f7b4de34990e945dbf62778a8846de5f`, respectively.

Intervals are 95% Wilson for rates, Newcombe hybrid for rate differences, and 4,000-draw percentile bootstrap intervals for ratios of means. McNemar is exact two-sided on task/trial pairs graded in both runs; wall and cost p-values are two-sided Mann–Whitney. A regression is a significant pass-rate loss, or a wall or cost ratio interval above 1 with p < 0.05. Three trials per cell leave substantial uncertainty. Task slowdown notes are descriptive: they flag a mean at least 1.5× and 30 seconds higher, or a paired trial at least 2× and 60 seconds higher. Costs include billed and estimated usage; estimates can be floors.

Two Luna setup probes were excluded: the candidate harness could not connect to the OLD daemon (protocol 26 versus 24), and an initial NEW run was stopped to correct its individual budget. Their private Aether databases recorded approximately $0.002024 and $0.006364, respectively; both are included in the combined model guard. The failed protocol cell is an infrastructure error, not a graded outcome. Agents ran without the optional systemd resource scope because the isolated environment had no user bus. The Luna guard log includes one intentional monitor restart; it did not interrupt a valid trial. The excluded Haiku serial runs recorded $1.227877, included in the combined Haiku budget. Luna used the original per-run agent temp roots, while later runs use shorter isolated roots; the recorded candidate binary did not change.

The trial records and grading evidence are in `runs/`; occurrences of the configured Git email are redacted in copied logs. `summary.json` contains the numerical results and per-run binary hashes; `index.html` is the visual report. Scripts in `scripts/` document the run and analysis. From the repository root, regenerate the numeric and visual reports with `python3 bench/results-full-1.6.5/scripts/analyze.py bench/results-full-1.6.5/runs bench/results-full-1.6.5`, then run `render.py` and `markdown.py` with `bench/results-full-1.6.5` as their argument.
