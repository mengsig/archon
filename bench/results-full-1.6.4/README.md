# Full suite: Archon v1.6.4 vs v1.6.3 (and next), Aether 1.4.2

**Verdict: NO REGRESSION** for Archon v1.6.4 against v1.6.3 on the full suite.

- No significant drop in pass rate and no significant rise in wall time on either model.

- NEXT (c17af3fe, #459) vs NEW, Haiku (claude-code/claude-haiku-5-5): no significant pass loss, not faster, cheaper (neither difference is significant).
- NEXT (c17af3fe, #459) vs NEW, Luna (codex/gpt-6-luna): no significant pass loss, not faster, cheaper (neither difference is significant).

Intervals are 95%: Wilson for a rate, Newcombe hybrid for a difference, a 4000-draw percentile bootstrap for a ratio of means. McNemar is exact two-sided on (task, trial) pairs both graded. Wall p is Mann-Whitney on pooled trials. Three trials per cell cannot separate small effects.

### Notes

- Two Rust tasks, multi-repo-currency and refactor-order-status, were void in the first pass of all six runs: the sandbox PATH had no cargo, so the grader and the agents could not run it. They were rerun for every arm (3 trials, cargo on PATH); only the rerun cells are counted.
- On Luna, long-backlog fell from 3/3 on OLD to 1/3 on both NEW and NEXT: the failed trials report a work-item branch that conflicted and was left out of grading. The Aether control passed 3/3 on every run. Pooled over the suite the drop is not significant (McNemar p=1.0), but it is a per-task flip worth a look.
- NEW was not slower or dearer than OLD in this suite (wall ratio 0.97x on Haiku, 0.84x on Luna; no significant difference), so #459 had no 1.6.4 slowdown to recover. NEXT changes reviewers per item (Haiku 1.65 to 1.47) and cost per trial slightly (0.93x on Haiku); neither wall nor cost difference is significant, and Luna NEXT wall is 1.13x NEW (not significant).
- Three trials per task give wide intervals: a +/-12 to 18 point difference is within noise. Luna costs include estimates for unbilled usage and are floors. The $25 (OLD, NEW) and $38 (NEXT) guards never tripped; each run spent under $3.70 (voided Rust trials included).

## Haiku (claude-code/claude-haiku-5-5)

### Arms

| Arm | Pass / graded (planned) | Pass % | 95% interval | Wall mean / median | Cost per trial | Stage turns per item | Reviewers per item | Infra errors |
|---|---|---:|---|---|---|---:|---:|---:|
| Archon OLD v1.6.3 | 32/33 of 33 | 97.0% | 84.7–99.5% | 181 s / 171 s | $0.0777 (33 priced) | 1.21 | 2.02 | 0 |
| Archon NEW v1.6.4 | 32/33 of 33 | 97.0% | 84.7–99.5% | 175 s / 160 s | $0.0692 (33 priced) | 1.12 | 1.65 | 0 |
| Archon NEXT c17af3fe | 32/33 of 33 | 97.0% | 84.7–99.5% | 181 s / 120 s | $0.0642 (33 priced) | 1.16 | 1.47 | 0 |
| Direct Aether control (run with OLD v1.6.3) | 33/33 of 33 | 100.0% | 89.6–100.0% | 53 s / 50 s | $0.0144 (33 priced) | 0.00 | 0.00 | 0 |
| Direct Aether control (run with NEW v1.6.4) | 33/33 of 33 | 100.0% | 89.6–100.0% | 58 s / 55 s | $0.0153 (33 priced) | 0.00 | 0.00 | 0 |
| Direct Aether control (run with NEXT c17af3fe) | 32/33 of 33 | 97.0% | 84.7–99.5% | 53 s / 47 s | $0.0144 (33 priced) | 0.00 | 0.00 | 0 |

### Comparisons

| Comparison (Archon arm) | Pass difference, 95% interval | McNemar baseline-only / candidate-only pass | McNemar exact p | Wall ratio [interval] (Mann-Whitney p) | Cost ratio [interval] (Mann-Whitney p) |
|---|---|---|---:|---|---|
| NEW vs OLD | +0.0 pp [-12.5, +12.5] | 1 / 1 of 33 | 1.000 | 0.97× [0.67, 1.38] (p=0.822) | 0.89× [0.62, 1.25] (p=0.603) |
| NEXT vs NEW | +0.0 pp [-12.5, +12.5] | 1 / 1 of 33 | 1.000 | 1.03× [0.66, 1.63] (p=0.501) | 0.93× [0.66, 1.30] (p=0.724) |
| NEW vs OLD (Aether control) | +0.0 pp [-10.4, +10.4] | 0 / 0 of 33 | 1.000 | 1.09× [0.85, 1.39] (p=0.640) | 1.06× [0.85, 1.32] (p=0.842) |
| NEXT vs NEW (Aether control) | -3.0 pp [-15.3, +7.7] | 1 / 0 of 33 | 1.000 | 0.91× [0.71, 1.18] (p=0.603) | 0.94× [0.76, 1.18] (p=0.715) |

### Per task (Archon)

| Task | OLD pass | NEW pass | NEXT pass | OLD wall | NEW wall | NEXT wall | NEW/OLD wall | NEXT/NEW wall | NEW stages / reviewers per item |
|---|---|---|---|---:|---:|---:|---:|---:|---|
| ask-cache-invalidation | 3/3 | 3/3 | 3/3 | 22 s | 32 s | 31 s | 1.46× | 0.96× | 0.0 / 0.0 |
| feature-token-bucket | 3/3 | 3/3 | 3/3 | 254 s | 154 s | 134 s | 0.61× | 0.87× | 1.0 / 1.3 |
| fix-off-by-one | 3/3 | 3/3 | 3/3 | 66 s | 64 s | 64 s | 0.97× | 1.01× | 1.0 / 1.0 |
| long-backlog | 3/3 | 3/3 | 3/3 | 153 s | 324 s | 549 s | 2.12× | 1.69× | 1.1 / 1.0 |
| multi-repo-currency | 3/3 | 3/3 | 3/3 | 227 s | 146 s | 132 s | 0.64× | 0.90× | 1.0 / 1.0 |
| multi-repo-pagination | 2/3 | 2/3 | 2/3 | 306 s | 264 s | 206 s | 0.86× | 0.78× | 1.0 / 3.0 |
| quality-ledger-transfer | 3/3 | 3/3 | 3/3 | 236 s | 138 s | 189 s | 0.59× | 1.37× | 1.0 / 1.7 |
| quality-retry-budget | 3/3 | 3/3 | 3/3 | 139 s | 125 s | 112 s | 0.91× | 0.89× | 1.0 / 1.0 |
| quality-safe-extract | 3/3 | 3/3 | 3/3 | 337 s | 390 s | 259 s | 1.16× | 0.66× | 1.0 / 4.0 |
| refactor-order-status | 3/3 | 3/3 | 3/3 | 224 s | 255 s | 277 s | 1.14× | 1.09× | 2.3 / 1.3 |
| review-seeded-bug | 3/3 | 3/3 | 3/3 | 32 s | 35 s | 34 s | 1.11× | 0.96× | 0.0 / 0.0 |

### Infrastructure errors

- Haiku (claude-code/claude-haiku-5-5): none.

### Missing cells

- Haiku (claude-code/claude-haiku-5-5): every planned trial ran.

### Flipped or slower tasks

- **Haiku (claude-code/claude-haiku-5-5), NEW vs OLD: long-backlog slower** (153 s to 324 s, 2.12×, p=0.050). Trials of the later arm:
  - trial 1 (pass, 416 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1. First error: none recorded
  - trial 2 (pass, 190 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=0; direct [implement#1:completed/unknown, implement#2:completed/unknown] reviewers=2. First error: none recorded
  - trial 3 (pass, 366 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1. First error: none recorded
- **Haiku (claude-code/claude-haiku-5-5), NEXT vs NEW: long-backlog slower** (324 s to 549 s, 1.69×, p=0.513). Trials of the later arm:
  - trial 1 (pass, 1077 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1. First error: none recorded
  - trial 2 (pass, 452 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1. First error: none recorded
  - trial 3 (pass, 118 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=0; direct [implement#1:completed/unknown] reviewers=1. First error: none recorded

## Luna (codex/gpt-6-luna)

### Arms

| Arm | Pass / graded (planned) | Pass % | 95% interval | Wall mean / median | Cost per trial | Stage turns per item | Reviewers per item | Infra errors |
|---|---|---:|---|---|---|---:|---:|---:|
| Archon OLD v1.6.3 | 31/33 of 33 | 93.9% | 80.4–98.3% | 257 s / 230 s | $0.0262 (33 priced) | 1.15 | 1.55 | 0 |
| Archon NEW v1.6.4 | 30/33 of 33 | 90.9% | 76.4–96.9% | 216 s / 194 s | $0.0275 (33 priced) | 1.36 | 1.48 | 0 |
| Archon NEXT c17af3fe | 30/33 of 33 | 90.9% | 76.4–96.9% | 243 s / 195 s | $0.0260 (33 priced) | 1.33 | 1.43 | 0 |
| Direct Aether control (run with OLD v1.6.3) | 30/33 of 33 | 90.9% | 76.4–96.9% | 58 s / 55 s | $0.0038 (33 priced) | 0.00 | 0.00 | 0 |
| Direct Aether control (run with NEW v1.6.4) | 32/33 of 33 | 97.0% | 84.7–99.5% | 60 s / 60 s | $0.0041 (33 priced) | 0.00 | 0.00 | 0 |
| Direct Aether control (run with NEXT c17af3fe) | 31/33 of 33 | 93.9% | 80.4–98.3% | 60 s / 60 s | $0.0040 (33 priced) | 0.00 | 0.00 | 0 |

### Comparisons

| Comparison (Archon arm) | Pass difference, 95% interval | McNemar baseline-only / candidate-only pass | McNemar exact p | Wall ratio [interval] (Mann-Whitney p) | Cost ratio [interval] (Mann-Whitney p) |
|---|---|---|---:|---|---|
| NEW vs OLD | -3.0 pp [-18.2, +11.8] | 3 / 2 of 33 | 1.000 | 0.84× [0.58, 1.22] (p=0.595) | 1.05× [0.73, 1.50] (p=0.753) |
| NEXT vs NEW | +0.0 pp [-15.7, +15.7] | 2 / 2 of 33 | 1.000 | 1.13× [0.73, 1.77] (p=0.893) | 0.95× [0.68, 1.34] (p=0.832) |
| NEW vs OLD (Aether control) | +6.1 pp [-7.6, +20.8] | 1 / 3 of 33 | 0.625 | 1.03× [0.82, 1.29] (p=0.763) | 1.07× [0.86, 1.32] (p=0.551) |
| NEXT vs NEW (Aether control) | -3.0 pp [-16.8, +10.0] | 2 / 1 of 33 | 1.000 | 1.00× [0.80, 1.24] (p=0.995) | 0.97× [0.77, 1.21] (p=0.863) |

### Per task (Archon)

| Task | OLD pass | NEW pass | NEXT pass | OLD wall | NEW wall | NEXT wall | NEW/OLD wall | NEXT/NEW wall | NEW stages / reviewers per item |
|---|---|---|---|---:|---:|---:|---:|---:|---|
| ask-cache-invalidation | 3/3 | 3/3 | 3/3 | 32 s | 31 s | 28 s | 0.99× | 0.91× | 0.0 / 0.0 |
| feature-token-bucket | 3/3 | 3/3 | 3/3 | 356 s | 252 s | 158 s | 0.71× | 0.63× | 1.0 / 2.0 |
| fix-off-by-one | 3/3 | 3/3 | 3/3 | 141 s | 136 s | 212 s | 0.97× | 1.55× | 1.0 / 1.0 |
| long-backlog | 3/3 | 1/3 | 1/3 | 444 s | 390 s | 288 s | 0.88× | 0.74× | 1.2 / 1.7 |
| multi-repo-currency | 3/3 | 3/3 | 3/3 | 240 s | 308 s | 208 s | 1.28× | 0.67× | 1.2 / 1.2 |
| multi-repo-pagination | 2/3 | 3/3 | 2/3 | 248 s | 219 s | 749 s | 0.89× | 3.42× | 1.0 / 2.0 |
| quality-ledger-transfer | 2/3 | 3/3 | 3/3 | 531 s | 206 s | 217 s | 0.39× | 1.05× | 1.3 / 1.0 |
| quality-retry-budget | 3/3 | 3/3 | 3/3 | 209 s | 157 s | 172 s | 0.75× | 1.10× | 1.0 / 0.7 |
| quality-safe-extract | 3/3 | 3/3 | 3/3 | 311 s | 280 s | 251 s | 0.90× | 0.89× | 1.3 / 3.0 |
| refactor-order-status | 3/3 | 3/3 | 3/3 | 279 s | 366 s | 350 s | 1.31× | 0.96× | 4.0 / 0.0 |
| review-seeded-bug | 3/3 | 2/3 | 3/3 | 33 s | 30 s | 43 s | 0.91× | 1.42× | 0.0 / 0.0 |

### Infrastructure errors

- Luna (codex/gpt-6-luna): none.

### Missing cells

- Luna (codex/gpt-6-luna): every planned trial ran.

### Flipped or slower tasks

- **Luna (codex/gpt-6-luna), NEW vs OLD: long-backlog lost passes** (3/3 to 1/3). Failed trials:
  - trial 2 (fail, 361 s): direct [implement#1:completed/unknown, implement#2:completed/unknown] reviewers=3; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1. First error: long-backlog: branches ["archon/zty0y4pj-tt-103-search-query-syntax"] conflicted and were left out
  - trial 3 (fail, 562 s): direct [implement#1:completed/unknown, implement#1:completed/unknown, implement#1:completed/unknown] reviewers=6; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=1. First error: long-backlog: branches ["archon/gvsc24er-tt-104-undo"] conflicted and were left out
- **Luna (codex/gpt-6-luna), NEW vs OLD: review-seeded-bug lost passes** (3/3 to 2/3). Failed trials:
  - trial 3 (fail, 25 s): no item. First error: none recorded
- **Luna (codex/gpt-6-luna), NEXT vs NEW: multi-repo-pagination lost passes** (3/3 to 2/3). Failed trials:
  - trial 1 (fail, 1802 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown, implement#1:cancelled/None] reviewers=6. First error: stage implement {'state': 'cancelled'}
- **Luna (codex/gpt-6-luna), NEXT vs NEW: fix-off-by-one slower** (136 s to 212 s, 1.55×, p=0.513). Trials of the later arm:
  - trial 1 (pass, 128 s): direct [implement#1:completed/unknown] reviewers=1. First error: none recorded
  - trial 2 (pass, 170 s): direct [implement#1:completed/unknown] reviewers=2. First error: none recorded
  - trial 3 (pass, 336 s): direct [implement#1:completed/unknown, implement#1:completed/unknown, implement#1:completed/unknown] reviewers=3. First error: none recorded
- **Luna (codex/gpt-6-luna), NEXT vs NEW: multi-repo-pagination slower** (219 s to 749 s, 3.42×, p=0.513). Trials of the later arm:
  - trial 1 (fail, 1802 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown, implement#1:cancelled/None] reviewers=6. First error: stage implement {'state': 'cancelled'}
  - trial 2 (pass, 200 s): direct [implement#1:completed/unknown] reviewers=2; direct [implement#1:completed/unknown] reviewers=1. First error: none recorded
  - trial 3 (pass, 246 s): direct [implement#1:completed/unknown] reviewers=1; direct [implement#1:completed/unknown] reviewers=2. First error: none recorded

## Method and provenance

- Arms: OLD = published Archon v1.6.3, NEW = published v1.6.4, NEXT = Archon built with `cargo build --release --locked -p archon`
  from origin/next `c17af3feba8f5a6a2adf480d77d4477894d6eccc` (1.6.4 plus #459). All three on the published Aether v1.4.2
  (`x86_64-unknown-linux-musl`). Every downloaded archive passed its release's `SHA256SUMS`.
- Harness: OLD used `archon-bench` built from the `v1.6.3` tag (the head's harness writes a config 1.6.3 refuses). NEW and NEXT used
  next's harness (built at c17af3fe), which has #459's fix for helper `agent.created` events. NEW and NEXT therefore share a harness;
  OLD does not, so the NEW vs OLD time comparison also carries any harness difference (`bench/tasks` and graders are identical).
- Every task under `bench/tasks` (11), 3 trials per (task, arm), medium effort, `archon` and direct `aether` arms, one trial at a time
  (`--parallel 1`), in a private sandbox per run (own HOME, config, data, temp and sockets). Six runs ran side by side, with the Aether
  release bench and a ccso stack on the same machine, so wall times carry shared-machine noise (load5 stayed below 9).
- Guard: `--max-usd 25` for OLD and NEW, 38 for NEXT; no run came near it (spent $1.25 to $3.62 each, the voided Rust trials and the rerun included).
- Stage turns per item count the stage records of each Archon item, every attempt; reviewers per item count agents the stage leads
  delegated (`archon-items.json`). An item with no `delegations` record counts as none. Cost is billed plus estimated dollars.
- Verdict rule: REGRESSION if, on either model, the Archon pass-rate difference interval lies entirely below 0, McNemar p < 0.05 with more
  pass-to-fail pairs, or wall time rose with the bootstrap ratio interval above 1 and Mann-Whitney p < 0.05.

| Binary | SHA256 |
|---|---|
| Archon 1.6.3 (published) | `8fe4927e3bc2d0c66e590e9fab58de8aca468cdfdc1345b07d3903da62b94981` |
| Archon 1.6.4 (published) | `c82005af40e2aed8fe2dae7d10d96d29b38228d5af2268c5f671ef965cf65f5c` |
| Archon NEXT c17af3fe (built) | `ce0e00452467a350db85e224c69a4929eb4b96dcb1a5d8fc2395f91027f4e644` |
| Aether 1.4.2 (published) | `c0510fe9d0669cf8eda7cc3f3154db21eb62c297b24b30896244e65398ab9a88` |
| archon-bench from v1.6.3 | `590163a9c0cd1ae1515d08a8f31d513ce3c05f00616b177323d261a58cbace5a` |
| archon-bench from next c17af3fe | `66764f52631de682f589a4cc14ec4b09464c980ddea8f923cf724d1f8eca737b` |

## Files

- `summary.json`: every number in this file. `runs/<arm>-<model>` is the full first pass; `runs/<arm>-<model>-rust` is the rerun of
  the two Rust tasks (the first-pass cells of those tasks are not counted). Each has `results.jsonl`, `run.json`, `summary.md`,
  `start.json` (binary hashes, load5, times) and the Archon item captures and review answers.
- `scripts/` holds the wrapper that ran each private sandbox (`run.py`) and the analysis (`analyze.py`, `render.py`) as they were run,
  against the working layout `<label>/results/<stamp>` rather than `runs/`. Credentials were copied mode 600 into each sandbox and
  deleted with it.
- The Lavish page was published as the run artifact `bench-report/index.html`.
