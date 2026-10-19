# Release 1.4.0 smoke gate

**Verdict: REGRESSION (Luna).** On `codex/gpt-6-luna`, next loses
`review-seeded-bug`, which v1.3.1 and the Luna baseline both pass: Archon 0/1
in both next runs (rubric score 0.5) against 1/1 for v1.3.1 and 1/1 for the
baseline. The gate run also lost `quality-ledger-transfer` (0/1, passed in the
repeat). Next's Luna gate run is Archon **2/4** against **3/4** for v1.3.1 and
**4/4** for the baseline. On Luna, next also cost and took more than v1.3.1
on the same aetherd (Archon cost +72% and +52%, trial wall +115% and +57%
over two runs), though not more than the baseline. On
`claude-code/claude-haiku-5-5` next passes: 4/4 in both runs, cheaper and
faster than v1.3.1 and the baseline.

Quality smoke preset (`archon-bench smoke`): `fix-off-by-one`,
`quality-ledger-transfer`, `quality-retry-budget`, `review-seeded-bug`; arms
`archon` and single-agent `aether`; one trial per cell, serial, medium
effort, $7 admission limit per run. One trial per cell is a regression
signal, not a reliability estimate.

## Versions

| | |
|---|---|
| next (measured) | archon 1.4.0, commit `600ddae3` (origin/next `321618e2` + origin/main `db1d311e` + version bump) |
| last release | archon 1.3.1, tag `v1.3.1` = `a3501e5d`, built from a clean export of the tag with its own target directory |
| harness | `archon-bench` from `600ddae3` for all runs; `run.json`'s `archon_commit` names this harness checkout, not the measured binary |
| aetherd | mengsig/aether origin/next `90aff1fa`, `cargo build --release -p aether` (reports 1.1.1, serves protocol 24); one aetherd served every run |
| engines | Claude Code 2.1.299, codex-cli 0.163.0 |
| binaries (sha256) | archon 1.4.0 `dfd48001…`, archon 1.3.1 `ca4d1089…`, archon-bench `bfcbe8da…`, aether `73d4f023…` |

Sandbox: one private `archon-bench/` sandbox below the data home, every
process started with `env -i` and only HOME, USER, LANG, PATH, TMPDIR and the
private ARCHON_/AETHER_ CONFIG, DATA and SOCKET variables (four distinct
directories). No operator daemon was used; the sandbox aetherd was stopped
after the last run and no sandbox process remained.

The aetherd's `archon` MCP server is one long-lived `archon mcp` process. A
first attempt at the two v1.3.1 runs could not replace its binary ("Text file
busy") and so measured 1.4.0 again; those runs are kept below as the repeat
next runs (`next ×2`). The v1.3.1 runs were then redone after replacing the
binary by rename and `aether mcp refresh archon`, with the running MCP
process's executable hash checked against the v1.3.1 build.

## Runs

Each cell: pass · trial wall s · cost $ (billed plus Aether's estimate; `≥`
marks an Archon trial whose cost Aether reported as incomplete).

### Haiku 5.5

| Run | Archon | Aether | Archon cost | Archon wall | End to end |
|---|---|---|---|---|---|
| next `20261019T062102Z` | 4/4 | 4/4 | $0.1015 | 326 s | 663 s |
| next ×2 `20261019T064854Z` | 4/4 | 4/4 | ≥ $0.1049 | 326 s | 652 s |
| v1.3.1 `20261019T071412Z` | 4/4 | 4/4 | $0.1716 | 461 s | 794 s |
| baseline `20261015T071250Z` | 4/4 | 4/4 | ≥ $0.1438 | 370 s | 11.5 min |

| Task (Archon) | next | next ×2 | v1.3.1 |
|---|---|---|---|
| fix-off-by-one | pass · 75 · 0.0317 | pass · 60 · 0.0217 | pass · 70 · 0.0276 |
| quality-ledger-transfer | pass · 120 · 0.0326 | pass · 120 · ≥ 0.0431 | pass · 255 · 0.1068 |
| quality-retry-budget | pass · 95 · 0.0278 | pass · 105 · 0.0297 | pass · 100 · 0.0281 |
| review-seeded-bug | pass · 35 · 0.0095 | pass · 40 · 0.0103 | pass · 35 · 0.0091 |

Aether arm: 4/4 in every run, $0.0396, $0.0349 and $0.0354, 155 s, 144 s and
151 s.

### Luna (`codex/gpt-6-luna`)

| Run | Archon | Aether | Archon cost | Archon wall | End to end |
|---|---|---|---|---|---|
| next `20261019T063205Z` | **2/4** | 4/4 | ≥ $0.0755 | 681 s | 1009 s |
| next ×2 `20261019T065946Z` | **3/4** | 4/4 | ≥ $0.0667 | 496 s | 823 s |
| v1.3.1 `20261019T072727Z` | 3/4 | 4/4 | ≥ $0.0438 | 316 s | 688 s |
| baseline `20261015T130836Z` | 4/4 | 4/4 | ≥ $0.0760 | 737 s | 19.1 min |

| Task (Archon) | next | next ×2 | v1.3.1 |
|---|---|---|---|
| fix-off-by-one | pass · 187 · ≥ 0.0260 | pass · 130 · ≥ 0.0138 | pass · 125 · ≥ 0.0130 |
| quality-ledger-transfer | **fail** · 288 · 0.0244 | pass · 200 · ≥ 0.0299 | pass · 125 · ≥ 0.0207 |
| quality-retry-budget | pass · 175 · ≥ 0.0187 | pass · 135 · ≥ 0.0193 | fail · 40 · 0.0048 |
| review-seeded-bug | **fail** · 30 · 0.0063 | **fail** · 30 · 0.0037 | pass · 25 · 0.0053 |

Aether arm: 4/4 in every run, $0.0132, $0.0128 and $0.0123, 145 s, 145 s and
190 s.

No run had a timeout or an infrastructure failure, so none was rerun.

## Next against v1.3.1 and the baselines

| Model | Check | next | v1.3.1 | baseline | Result |
|---|---|---|---|---|---|
| Haiku | Archon passes | 4/4, 4/4 | 4/4 | 4/4 | kept |
| Haiku | Archon cost | $0.1015, $0.1049 | $0.1716 | $0.1438 | −41% vs v1.3.1, −29% vs baseline |
| Haiku | Archon trial wall | 326 s, 326 s | 461 s | 370 s | −29% vs v1.3.1, −12% vs baseline |
| Luna | Archon passes | 2/4, 3/4 | 3/4 | 4/4 | **review-seeded-bug lost** (both next runs); ledger lost in the gate run |
| Luna | Archon cost | $0.0755, $0.0667 | $0.0438 | $0.0760 | +72% / +52% vs v1.3.1; −1% / −12% vs baseline |
| Luna | Archon trial wall | 681 s, 496 s | 316 s | 737 s | +115% / +57% vs v1.3.1; −8% / −33% vs baseline |

## The Luna failures

- `review-seeded-bug` (both next runs): the orchestrator answered itself with
  an empty queue and named the bug correctly (`expected.startsWith(signature)`
  accepts any prefix of the HMAC, including an empty signature, so `body.`
  verifies), but neither answer used the words the rubric's second group looks
  for ("forg", "bypass", "any payload"), so it scored 0.5. v1.3.1's answer
  called it "an authentication bypass" and passed. This matches the lexical
  miss recorded for the earlier `luna-no-baseline-20261015` attempt; the
  grader is unchanged here.
- `quality-ledger-transfer` (next gate run): the `direct` implement stage
  committed a working ledger and its tests passed, but its result carried no
  structured ship record. The stage agent reported that Aether's `delegate`,
  `wait_for`, `complete_task` and `fail_task` tools were not offered to it, so
  it could neither run the review nor complete the task; archond failed the
  item and the graded `ledger.py` is the untouched stub. In the same run,
  `fix-off-by-one`'s first item failed because a reviewer's result carried no
  verdict and passed on a second item. Neither happened on Haiku or in the
  v1.3.1 Luna run.
- `quality-retry-budget` (v1.3.1, for comparison): the orchestrator
  implemented the change in its own mirror without queueing an item, so the
  graded repository was untouched.
