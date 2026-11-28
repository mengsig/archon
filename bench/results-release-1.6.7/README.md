# Archon 1.6.7 release benchmark

**Verdict: NO REGRESSION against 1.6.6 on either tier.** Haiku's Archon arm passed **29/33 on 1.6.6 and 32/33 on 1.6.7**, with cost 0.73× and wall time 0.76×. Luna's passed **30/33 on both**, with cost 1.07× and wall time 1.23×. No difference is beyond noise: every pass-difference interval spans 0, both McNemar p ≥ 0.38, and every cost and wall p ≥ 0.43. The direct-Aether controls are flat (Haiku 33 → 32, Luna 33 → 33, cost and wall 0.99× to 1.02×).

**Accuracy parity with direct Aether: PARITY MISSED on Luna, MET on Haiku.** On the 1.6.7 arm, Haiku Archon passed 32/33 and Haiku Aether 32/33. Luna Archon passed 30/33 against Luna Aether's 33/33. On 1.6.6 both tiers missed parity: Haiku 29 against 33, Luna 30 against 33. Parity is reported but does not block the release on its own.

[Visual report](report.html) · [Numeric summary](summary.json) · [Wall-time data](walltime.json)

OLD is published Archon v1.6.6 with Aether v1.4.3. NEW is the 1.6.7 candidate `acaad050` with Aether v1.4.4. The candidate includes #511, the fix for Archon trailing plain Aether. Both ran all 11 tasks, 3 trials, arms `archon` and `aether`, effort `high`, on two tiers: Haiku (`claude-code/claude-haiku-5-5`) and Luna (`codex/gpt-6-luna`). Every planned trial ran, 66 per run, with no infrastructure errors. The Codex quota held, and no Luna trial is missing.

The NEW arms include the restored 1.6.4 delta-reviewer rule: a delta reviewer fails only on a defect in its commits or an open finding left unfixed. A new finding in code that a pass already read is non-blocking. So NEW measures this review-gate change together with #511, Aether 1.4.4 and the #508/#509 code.

`make ci`: **PASS.** `CARGO_BUILD_JOBS=6 make ci` ran through `scripts/ci-slot.sh` on `acaad050` (tree `e4cf51f3`), 2026-11-27 21:59 – 22:14 UTC: exit 0, 5,803 tests passed, 0 failed. The final head has the same tree, so the result holds for it.

## Results per tier

| Tier | Arm | 1.6.6 pass | 1.6.7 pass | Rate diff, pp [95%] | McNemar old-only/new-only | $ / trial (ratio [95%]) | Min / trial (ratio [95%]) | Timeouts |
|---|---|---:|---:|---|---|---|---|---|
| Haiku | archon | 29/33 | 32/33 | +9.1 [-5.2, +24.5] | 1/4, p=0.38 | $0.1338 → $0.0973 (0.73× [0.48, 1.12], p=0.48) | 5.5 → 4.2 (0.76× [0.49, 1.20], p=0.43) | 4 → 0 |
| Haiku | aether | 33/33 | 32/33 | -3.0 [-15.3, +7.7] | 1/0, p=1.00 | $0.0199 → $0.0197 (0.99× [0.79, 1.24], p=0.97) | 1.4 → 1.4 (1.00× [0.78, 1.27], p=0.89) | 0 → 0 |
| Luna | archon | 30/33 | 30/33 | +0.0 [-15.7, +15.7] | 3/3, p=1.00 | $0.0262 → $0.0280 (1.07× [0.73, 1.56], p=0.80) | 4.0 → 5.0 (1.23× [0.85, 1.79], p=0.67) | 0 → 1 |
| Luna | aether | 33/33 | 33/33 | +0.0 [-10.4, +10.4] | 0/0, p=1.00 | $0.0049 → $0.0050 (1.02× [0.80, 1.31], p=0.74) | 1.5 → 1.5 (1.00× [0.79, 1.25], p=0.98) | 0 → 0 |

Private-daemon spend, from each run's usage database: Haiku $5.08 (OLD) and $3.86 (NEW), Luna $1.02 and $1.09 (`runs/budget.json`).

### Archon per task

| Task | Haiku 1.6.6 → 1.6.7 | Haiku $ / trial | Haiku min | Luna 1.6.6 → 1.6.7 | Luna $ / trial | Luna min |
|---|---|---|---|---|---|---|
| `ask-cache-invalidation` | 3/3 → 3/3 | 0.010 → 0.011 | 0.6 → 0.6 | 2/3 → **3/3** | 0.006 → 0.007 | 1.0 → 0.9 |
| `feature-token-bucket` | 3/3 → 3/3 | 0.086 → 0.083 | 5.0 → 5.0 | 3/3 → 3/3 | 0.019 → 0.026 | 3.6 → 4.4 |
| `fix-off-by-one` | 3/3 → 3/3 | 0.029 → 0.032 | 1.5 → 1.6 | 3/3 → 3/3 | 0.015 → 0.011 | 2.7 → 2.0 |
| `long-backlog` | 3/3 → 3/3 | 0.158 → 0.144 | 3.5 → 3.1 | 1/3 → **2/3** | 0.069 → 0.061 | 6.4 → 6.6 |
| `multi-repo-currency` | 3/3 → 3/3 | 0.117 → 0.086 | 3.8 → 3.0 | 3/3 → **2/3** | 0.043 → 0.030 | 6.6 → 4.3 |
| `multi-repo-pagination` | 3/3 → 3/3 | 0.202 → 0.117 | 6.7 → 3.8 | 3/3 → 3/3 | 0.021 → 0.053 | 2.8 → 8.0 |
| `quality-ledger-transfer` | 2/3 → **3/3** | 0.277 → 0.149 | 13.9 → 6.1 | 3/3 → **2/3** | 0.031 → 0.025 | 5.5 → 11.0 |
| `quality-retry-budget` | 3/3 → 3/3 | 0.041 → 0.043 | 2.2 → 2.2 | 3/3 → 3/3 | 0.017 → 0.018 | 3.3 → 3.1 |
| `quality-safe-extract` | 0/3 → **3/3** | 0.377 → 0.187 | 12.1 → 7.3 | 3/3 → 3/3 | 0.038 → 0.038 | 6.4 → 6.7 |
| `refactor-order-status` | 3/3 → 3/3 | 0.161 → 0.202 | 9.9 → 12.3 | 3/3 → 3/3 | 0.025 → 0.034 | 4.9 → 6.7 |
| `review-seeded-bug` | 3/3 → **2/3** | 0.013 → 0.017 | 0.9 → 0.8 | 3/3 → 3/3 | 0.005 → 0.006 | 1.2 → 1.1 |

### Pass-to-fail flips (1.6.6 passed, 1.6.7 failed, same task and trial)

- **Haiku `review-seeded-bug` #2:** wrong output. The Archon trial delivered no change (empty patch).
- **Luna `long-backlog` #2:** wrong output. The hidden tests failed. That task also flipped two trials the other way (1.6.6 passed 1/3, 1.6.7 2/3).
- **Luna `multi-repo-currency` #1:** wrong output. The hidden tests failed.
- **Luna `quality-ledger-transfer` #2:** timeout at 1,203 s, waiting on a parent that never replies (see the timeouts section below).
- **Direct-Aether control, Haiku `multi-repo-pagination`:** 3/3 → 2/3. It is the same Aether family on both arms, so this is the noise floor.

Fail-to-pass flips outnumber them: Haiku `quality-safe-extract` 0/3 → 3/3 and `quality-ledger-transfer` 2/3 → 3/3, Luna `ask-cache-invalidation` 2/3 → 3/3 and `long-backlog` 1/3 → 2/3. All four 1.6.6 Haiku failures were review-loop timeouts, which is what #511 and the restored delta rule target. Every flip is a single trial within three-trial cells, so none is significant alone. No confirming rerun was needed, because no regression was flagged (operator direction: skip confirming reruns unless a flagged regression needs one).

## Accuracy parity with direct Aether

At the operator's request, the report compares each tier's Archon arm with its direct-Aether arm on the same run: pass counts overall and per task. The verdict is PARITY MET when Archon passes at least as many trials as Aether. `summary.json` has the same data under `parity` (`scripts/analyze.py`).

| Tier | 1.6.6 Archon / Aether | 1.6.7 Archon / Aether | 1.6.7 verdict |
|---|---|---|---|
| Haiku | 29 / 33 | **32 / 32** | PARITY MET |
| Luna | 30 / 33 | **30 / 33** | PARITY MISSED |

Tasks where 1.6.7 Archon passed fewer trials than 1.6.7 Aether, with each Archon failure's reason from the trial log:

- **Haiku `review-seeded-bug` (Archon 2/3, Aether 3/3).** #2: wrong output, no change delivered (empty patch). Haiku Aether also lost one trial, on `multi-repo-pagination` (Archon 3/3 there), so the totals tie.
- **Luna `long-backlog` (2/3 vs 3/3).** #2: wrong output, the hidden tests failed.
- **Luna `multi-repo-currency` (2/3 vs 3/3).** #1: wrong output, the hidden tests failed.
- **Luna `quality-ledger-transfer` (2/3 vs 3/3).** #2: timeout. The implement lead read the spec. It took the bench brief's "escalate if it turns out risky (concurrency, locking)" clause as a question. It sent a raw `send_message` to its Aether parent, a run node, and called `wait_for` on it. That parent never replies, so the lead idled from 08:58:12 until the 1,203 s limit. Its contract says, in so many words, "Your Aether parent is a run node that never replies: never ask or `wait_for` it", and the stored agent record confirms that the lead had that text. This is a model compliance failure on a path #511 already addresses in prompt text. Aether 1.4.4 still accepts a `wait_for` on a parent that can never answer. Refusing or timing out that wait in Aether would close it whatever the model does (a suggested follow-up for an Aether item).

None of the shortfalls is a grader or harness fault. No trial has an error, and every failed trial but the timeout has failing hidden tests or an empty patch in its grade log.

## Wall time and timeouts

This section uses stored results only; no extra runs were made. `walltime.json` has every number below, produced by `scripts/walltime.py`.

**Two confounds.** First, effort: 1.6.4 and 1.6.5 ran at `medium` effort (`bench/results-full-1.6.4`, `bench/results-full-1.6.5`), and this release runs at `high`, as the bench README specifies. Second, host load: this release ran all four runs side by side (below). The direct-Aether control has no Archon in the loop. It is 1.32× (Haiku) and 1.40× (Luna) slower here than in 1.6.5, and 1.43× and 1.52× slower than in 1.6.4. That much of any Archon rise since 1.6.5 is effort and load, not Archon. 1.6.4's Rust tasks (`multi-repo-currency`, `refactor-order-status`) come from its Rust rerun.

### Mean wall per trial, by arm

| Tier | Arm | 1.6.4 (medium) | 1.6.5 (medium) | 1.6.6 (high) | 1.6.7 (high) |
|---|---|---:|---:|---:|---:|
| Haiku | Archon | 175 s (96 min, 0 TO) | 241 s (133 min, 2 TO) | 328 s (180 min, 4 TO) | 250 s (138 min, 0 TO) |
| Haiku | Direct Aether | 58 s (32 min, 0 TO) | 63 s (34 min, 0 TO) | 83 s (46 min, 0 TO) | 83 s (46 min, 0 TO) |
| Luna | Archon | 216 s (119 min, 0 TO) | 248 s (136 min, 0 TO) | 242 s (133 min, 0 TO) | 299 s (164 min, 1 TO) |
| Luna | Direct Aether | 60 s (33 min, 0 TO) | 65 s (36 min, 0 TO) | 91 s (50 min, 0 TO) | 91 s (50 min, 0 TO) |

The minutes in brackets are the total over 33 trials. TO is the number of timed-out trials.

### Archon mean wall per task (seconds)

Haiku:

| Task | 1.6.4 | 1.6.5 | 1.6.6 | 1.6.7 |
|---|---:|---:|---:|---:|
| `ask-cache-invalidation` | 32 | 32 | 36 | 39 |
| `feature-token-bucket` | 154 | 234 | 302 | 303 |
| `fix-off-by-one` | 64 | 85 | 91 | 95 |
| `long-backlog` | 324 | 204 | 213 | 185 |
| `multi-repo-currency` | 146 | 202 | 228 | 178 |
| `multi-repo-pagination` | 264 | 270 | 400 | 229 |
| `quality-ledger-transfer` | 138 | 392 | 831 | 364 |
| `quality-retry-budget` | 125 | 329 | 135 | 132 |
| `quality-safe-extract` | 390 | 483 | 726 | 439 |
| `refactor-order-status` | 255 | 369 | 592 | 739 |
| `review-seeded-bug` | 35 | 50 | 52 | 50 |

Luna:

| Task | 1.6.4 | 1.6.5 | 1.6.6 | 1.6.7 |
|---|---:|---:|---:|---:|
| `ask-cache-invalidation` | 31 | 40 | 58 | 52 |
| `feature-token-bucket` | 252 | 197 | 216 | 263 |
| `fix-off-by-one` | 136 | 122 | 160 | 118 |
| `long-backlog` | 390 | 559 | 382 | 397 |
| `multi-repo-currency` | 308 | 328 | 395 | 260 |
| `multi-repo-pagination` | 219 | 292 | 169 | 479 |
| `quality-ledger-transfer` | 206 | 442 | 328 | 661 |
| `quality-retry-budget` | 157 | 212 | 198 | 184 |
| `quality-safe-extract` | 280 | 239 | 385 | 404 |
| `refactor-order-status` | 366 | 259 | 297 | 399 |
| `review-seeded-bug` | 30 | 40 | 69 | 67 |

### Every timed-out trial

A timed-out trial counts as a failure. The cause of each comes from the trial's `archon-items.json`, and for the 1.6.7 Luna trial from the run's private Aether event log.

| Release | Tier | Task | Arm | Trial | Wall (s) | Cause | Peak load5 |
|---|---|---|---|---:|---:|---|---:|
| 1.6.5 | Haiku | `quality-safe-extract` | archon | 1 | 721 | review loop: 9 reviewers, still reviewing at the limit | n/a |
| 1.6.5 | Haiku | `quality-retry-budget` | archon | 3 | 722 | implement lead still working at the limit, no reviewer delegated yet (long engine turns) | n/a |
| 1.6.6 | Haiku | `quality-ledger-transfer` | archon | 1 | 1233 | review loop: 5 reviewers, still reviewing at the limit | 14.5 |
| 1.6.6 | Haiku | `quality-safe-extract` | archon | 1 | 728 | review loop: 5 reviewers, still reviewing at the limit | 19.6 |
| 1.6.6 | Haiku | `quality-safe-extract` | archon | 2 | 722 | review loop: 4 reviewers, still reviewing at the limit | 11.2 |
| 1.6.6 | Haiku | `quality-safe-extract` | archon | 3 | 728 | review loop: 5 reviewers, still reviewing at the limit | 9.4 |
| 1.6.7 | Luna | `quality-ledger-transfer` | archon | 2 | 1203 | waiting: after 23 s the lead sent a raw `send_message` to its run-node parent asking whether locking needs an escalation, then called `wait_for` on it; that parent never replies, so it idled until the limit | 16.0 |

Causes, in short:

- **Review loop (all four 1.6.6 Haiku timeouts; 1.6.5 Haiku `quality-safe-extract` #1).** The stage was still in review rounds at the limit, with 4 to 9 reviewers delegated. 1.6.7 has none: Haiku `quality-safe-extract` went from 0/3 (726 s mean) to 3/3 (439 s), and `quality-ledger-transfer` from 831 s to 364 s. That is the mechanism #511 and the restored delta rule change.
- **Waiting (1.6.7 Luna `quality-ledger-transfer` #2).** The lead asked its run-node parent instead of using `question`, which is traced above.
- **Long engine turns (1.6.5 Haiku `quality-retry-budget` #3).**
- No timeout was infrastructure; every trial in both runs has `error: null`.
- Trials at about 721 s and 1,203 – 1,233 s are the harness's per-task limits, not hung processes.

### Is anything slower beyond noise?

- **1.6.7 against 1.6.6 (same host, same time, same flags).** Haiku Archon is faster: 0.76× [0.49, 1.20], p = 0.43. Luna Archon is slower, 1.23× [0.85, 1.79], p = 0.67: the interval spans 1, so this is not a regression by the report's rule. Two trials explain most of it. One is the waiting timeout above (1,203 s). The other is `multi-repo-pagination` #2 (1,136 s, passed): its server-side item (GET /items cursor pagination) went through review rounds with 11 reviewers over 14 minutes, then a 4-minute record-repair attempt, before it shipped. Haiku `refactor-order-status` (592 → 739 s) comes from one 1,146 s trial that passed. Both direct-Aether controls are 1.00×.
- **1.6.6 against 1.6.4.** Haiku Archon's mean went from 175 s to 328 s (1.87×), more than the control's 1.43× effort-and-load rise. That gap is the four 1.6.6 review-loop timeouts. It is a real 1.6.6 slowdown, and 1.6.7 removes it: 250 s, 1.43×, the same as the control. Luna Archon went 216 → 242 s (1.12×), below the control's 1.52×.
- **Verdict on wall time:** possible regression in **1.6.6 on Haiku** (review loops), which 1.6.7 fixes. No wall-time regression in 1.6.7. One Luna waiting timeout is worth watching; its cause is named above.

### Overlap with other host work

The host has 12 cores and 15 GB of memory. A load logger sampled `/proc/loadavg` every minute (`load5` below, `.work/load.log`, not committed).

| Job | UTC (2026-11-28) | Mean / peak load5 |
|---|---|---|
| Release build of the candidate (`cargo build --release --locked`, 6 jobs) | 07:13 – 07:19 | before the bench |
| All four runs side by side (OLD Haiku, OLD Luna, NEW Haiku, NEW Luna) | 07:20 – 10:01 | 11.8 / 21.7 |
| Other Archon items' `cargo test` / `cargo clippy` (`-j 2`, not this item's) | seen at about 08:10 – 08:40 | 15.7 / 21.0 |
| The last three, then two runs (OLD Luna ended 10:01, NEW Haiku 10:05, NEW Luna 10:34, OLD Haiku 10:45) | 10:01 – 10:45 | 9.5 / 12.8 |

All four runs ran at the same time, so OLD and NEW saw the same host load throughout (orchestrator-approved). The load was heavy: at times more than one runnable task per core. That inflates every arm's wall time, direct Aether's included, which is why wall time is compared within this run, OLD against NEW, and only loosely against 1.6.4 and 1.6.5. `make ci` did not overlap the bench. It ran on the same tree before it (2026-11-27 21:59 – 22:14 UTC). Other items' cargo jobs overlapped the middle of the bench, equally for all four runs.

**Why the whole bench ran again.** Earlier attempts of this item had run the OLD arms (66/66 each, 2026-11-27 08:25 – 12:47 UTC) and three NEW attempts. Two of those were on the pre-fix candidate `9c121cc2`. One was on the merged candidate, and a host sign-in expiry stopped it after 16 minutes. All their raw data lived in the worktree's gitignored `.archon/reports/bench`. A worktree cleanup of ignored files deleted it at 2026-11-27 23:39 UTC, before any of it was copied out or published. No row from those attempts is used or kept, so this report has no pre-fix data. Both arms of both tiers were rerun from scratch on 2026-11-28 with the same flags, from a directory that is not gitignored. Redacted snapshots were published as item artifacts while the runs went on.

## What in v1.6.6..HEAD can reach a bench run

- **#511, the fix for Archon trailing plain Aether** (`review/blockers.rs`, the findings and unreadable-result notes, the stage contract). It changes when a stage lead is told about open blockers, delivers review notes without interrupting the lead, and tells leads that their Aether parent never replies. It is on the path of every Archon trial that reaches review, on both tiers, and it is the main change the NEW arms measure.
- **Restored delta-reviewer rule** (`contract.rs` `REVIEW_ROUNDS_RULE`, `review/record/previous.rs`, `review/confirm.rs`). It is on the path of every Archon trial that reaches review, on both tiers.
- **Aether 1.4.4.** It affects both arms on both tiers.
- **#508 Antigravity provider and opt-in filters** (`model_tier.rs`, `learned/routing.rs`, `acceptance/spawn.rs`, `opt_in.rs`). These only exclude an opt-in provider's models unless policy names them. The sandbox has no `agy` and no Antigravity models, so on claude-code and codex they are no-ops.
- **#509 design sessions** (recipe, stack auto-start, Compose teardown, design contract text). These only run in design stages. The bench never creates one, and `design_recipe` is hidden from non-design agents.

## Method and provenance

- Each run used a private sandbox under `~/.local/share/archon-bench/release-167-R4V9TB`, removed afterwards. Each sandbox had a private `aetherd` on its own socket, private Archon and Aether data and config directories, a private `HOME`, and copies of the engine credentials (mode 600, deleted with the sandbox). Nothing touched the installed daemons.
- `--parallel 3` applies to the direct-Aether arm. Archon trials run one at a time inside a run.
- The bench harness is the same binary for OLD and NEW: `archon-bench` built at the candidate head. `bench/` is unchanged since v1.6.6, and the harness speaks to both Archon versions.
- The `head` in each run's `start.json`, and the "Archon commit" in its summary, is the reporting checkout's HEAD at launch. The binaries under test are the hashed ones below.
- Confirming reruns were skipped on operator direction (ship 1.6.7 as soon as possible). No regression was flagged, so none was needed.

| Binary | SHA-256 |
|---|---|
| Archon v1.6.6 (release binary) | `b8be4cf3786f9baa453bf0b0cc9c9e3850d680339828058e1c7b20fad2ddebb7` |
| Archon v1.6.6 archive | `99764e825399027601af9ad435de28fdb488fa90121561aeec8c11806fe7f135` |
| Candidate Archon 1.6.7 (`cargo build --release --locked` at `acaad050`) | `924b4a0f5e47260faeef3d3c4b92a0cd23eff62228ca2bcf2b1ece70ec9e0b29` |
| Aether v1.4.3 (release binary) | `3a795ff2a718e8c227ac66ad6bbefd2cfe1838bfcbf77b37eac10e56ca3cc923` |
| Aether v1.4.3 archive | `25cf7df3db41fdab660fc5a5781e82f0f7b4de34990e945dbf62778a8846de5f` |
| Aether v1.4.4 (release binary) | `9a874c054c49c931a6fc3cfd9e0ae92a76c37afeefcad3ea5f60470c1e391d1f` |
| Aether v1.4.4 archive | `8da7a9ffc55122fd8d85885c42504cb062816f99c96de76f1016954a12dfca4f` |
| `archon-bench` | `96dd736441e638e2ae2d0816ec598e78fc40a16d2c7683ef67b74773547e21e6` |

All three release archives (x86_64 musl) passed their release `SHA256SUMS` checks. Aether v1.4.4 is a published, non-draft release with `SHA256SUMS`, tagged on `9f81eed8`. The binaries report `archon 1.6.6`, `archon 1.6.7`, `aether 1.4.3` and `aether 1.4.4`, recorded in each run's `start.json`.

Statistics match the 1.6.5 report. Rates use 95% Wilson intervals, and rate differences use Newcombe hybrid intervals. McNemar is exact two-sided on paired task/trial cells. Wall-time and cost p-values are two-sided Mann–Whitney, and ratio intervals are 4,000-draw bootstrap percentiles. A regression is any of these:

- a pass-difference interval entirely below 0;
- McNemar p < 0.05 with more OLD-only passes;
- a cost or wall ratio interval above 1 with p < 0.05.

Three trials per cell leave wide intervals, and a single task flip is within noise. Costs are billed plus estimated usage, and estimates can be floors.

### Commands

From the repository root (`scripts/run.py` builds the sandbox and runs the harness):

```sh
cargo build --release --locked -p archon -p archon-bench
# all four side by side, each from the repository root:
python3 -I bench/results-release-1.6.7/scripts/run.py old haiku old-haiku
python3 -I bench/results-release-1.6.7/scripts/run.py old luna old-luna
python3 -I bench/results-release-1.6.7/scripts/run.py new haiku new-haiku
python3 -I bench/results-release-1.6.7/scripts/run.py new luna new-luna
```

No run used `--only`.


Each run executes:

```sh
archon-bench run --archon-bin <sandbox>/bin/archon --aether-socket <sandbox>/run/a.sock \
  --results-dir <out>/results --arms archon,aether --trials 3 --parallel 3 \
  --effort high --max-usd 37 --model <claude-code/claude-haiku-5-5|codex/gpt-6-luna>
```

Then the reports:

```sh
python3 -I bench/results-release-1.6.7/scripts/collect.py bench/results-release-1.6.7/.work bench/results-release-1.6.7/runs
python3 -I bench/results-release-1.6.7/scripts/analyze.py bench/results-release-1.6.7/runs bench/results-release-1.6.7
python3 -I bench/results-release-1.6.7/scripts/walltime.py bench/results-release-1.6.7 bench/results-release-1.6.7/.work/load.log > bench/results-release-1.6.7/walltime.json
python3 -I bench/results-release-1.6.7/scripts/report.py bench/results-release-1.6.7 "<make ci result>"
```

`runs/` holds the trial records and grading evidence. The configured Git email is redacted in copied logs.
