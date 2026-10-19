# Release 1.4.0 smoke gate

**Verdict: REGRESSION (Luna).** In the re-gate at `74a33e3a`, on
`codex/gpt-6-luna`, next passes **9/12** Archon trials over three runs against
**12/12** for v1.3.1. Next's third run scored 1/4: `fix-off-by-one` and
`quality-retry-budget` failed because the orchestrator made the change in its
own mirror and queued no item, and `review-seeded-bug` scored 0.5 on the
rubric (the Aether arm missed it in that run too). Per task, next has
`fix-off-by-one` 2/3, `quality-ledger-transfer` 3/3, `quality-retry-budget`
2/3 and `review-seeded-bug` 2/3, against 3/3 each for v1.3.1. No task is at
0/3. Next's median Archon cost is +15% and its median trial wall time +8% over
v1.3.1, both within the 25% bound. On `claude-code/claude-haiku-5-5` next
passes: Archon 4/4 against the baseline's 4/4, $0.1364 (−5%) and 416 s
(+12%). The rule fails only on total passes (9 < 12). Nothing was tuned
to pass.

The first gate's numbers and findings are kept below as history.

## Re-gate (2026-10-19, at `74a33e3a`)

`CARGO_BUILD_JOBS=6 make ci` at `74a33e3a` passed: fmt-check, clippy
`-D warnings`, doc and 2922 tests with 0 failed (9 ignored).

Same preset, method and $7 cap per run as the first gate: four tasks, arms
`archon` and `aether`, one trial per cell, serial, medium effort. Seven runs,
one at a time: Luna next, v1.3.1, next, v1.3.1, next, v1.3.1, then Haiku next.
No run had a timeout or an infrastructure failure, so none was rerun.

### Versions

| | |
|---|---|
| next (measured) | archon 1.4.0, commit `74a33e3a` |
| last release | archon 1.3.1, tag `v1.3.1` = `a3501e5d`, built from a clean export of the tag with its own target directory; its binary hash equals the first gate's v1.3.1 build |
| harness | `archon-bench` from `74a33e3a` for all runs; `run.json`'s `archon_commit` names this harness checkout, not the measured binary |
| aetherd | mengsig/aether origin/next, still `90aff1fa`: the first gate's binary (hash checked), reports 1.1.1 and serves protocol 24; one aetherd served all seven runs |
| engines | Claude Code 2.1.299, codex-cli 0.163.0 |
| binaries (sha256) | archon 1.4.0 `3b6ff2a1…`, archon 1.3.1 `ca4d1089…`, archon-bench `01cd9e65…`, aether `73d4f023…` |

Sandbox: a new private `archon-bench/` sandbox, set up as in the first gate
(`env -i` with only HOME, USER, LANG, PATH, TMPDIR and the private ARCHON_/
AETHER_ CONFIG, DATA and SOCKET variables, four distinct directories). No
operator daemon was used. Before each run the sandbox's `archon` was replaced
by rename with the build under test and `aether mcp refresh archon` restarted
the `archon mcp` server. The hash of the running MCP process's executable was
then checked against that build, and it matched before all seven runs. The
sandbox aetherd was stopped after the last run and no sandbox process
remained.

### Per run

Each cell: pass · trial wall s · cost $. Archon costs of the three code tasks
have `cost_gap = mixed` (`*` in the bench's report): incomplete costs plus
estimates, not a bound.

| Run | Build | fix-off-by-one | quality-ledger-transfer | quality-retry-budget | review-seeded-bug | Archon | Archon cost | Archon wall | End to end |
|---|---|---|---|---|---|---|---|---|---|
| `20261019T110722Z` | next | pass · 140 · 0.0246 | pass · 100 · 0.0178 | pass · 110 · 0.0222 | pass · 25 · 0.0042 | 4/4 | $0.0688 | 376 s | 643 s |
| `20261019T111810Z` | v1.3.1 | pass · 80 · 0.0164 | pass · 85 · 0.0174 | pass · 100 · 0.0167 | pass · 20 · 0.0035 | 4/4 | $0.0540 | 285 s | 578 s |
| `20261019T112759Z` | next | pass · 80 · 0.0134 | pass · 145 · 0.0265 | pass · 101 · 0.0182 | pass · 25 · 0.0047 | 4/4 | $0.0628 | 351 s | 654 s |
| `20261019T113857Z` | v1.3.1 | pass · 145 · 0.0259 | pass · 95 · 0.0141 | pass · 100 · 0.0164 | pass · 30 · 0.0042 | 4/4 | $0.0605 | 371 s | 668 s |
| `20261019T115010Z` | next | **fail** · 30 · 0.0033 | pass · 210 · 0.0385 | **fail** · 45 · 0.0078 | **fail** · 25 · 0.0041 | **1/4** | $0.0537 | 311 s | 604 s |
| `20261019T120017Z` | v1.3.1 | pass · 100 · 0.0160 | pass · 100 · 0.0152 | pass · 105 · 0.0198 | pass · 20 · 0.0036 | 4/4 | $0.0546 | 325 s | 628 s |

Aether arm: next 4/4, 4/4 and 3/4 (`review-seeded-bug` failed in the third
run); v1.3.1 4/4 in all three runs. Its cost was $0.0127–$0.0153 a run.

### Per build (Luna)

| | next | v1.3.1 | next vs v1.3.1 |
|---|---|---|---|
| Archon passes | **9/12** | 12/12 | −3 |
| fix-off-by-one | 2/3 | 3/3 | |
| quality-ledger-transfer | 3/3 | 3/3 | |
| quality-retry-budget | 2/3 | 3/3 | |
| review-seeded-bug | 2/3 | 3/3 | |
| median Archon cost | $0.0628 | $0.0546 | +15% |
| median Archon trial wall | 351 s | 325 s | +8% |
| median end to end | 643 s | 628 s | +2% |

### Haiku 5.5 against its baseline

| Run | fix-off-by-one | quality-ledger-transfer | quality-retry-budget | review-seeded-bug | Archon | Aether | Archon cost | Archon wall | End to end |
|---|---|---|---|---|---|---|---|---|---|
| next `20261019T121050Z` | pass · 40 · 0.0209 | pass · 230 · 0.0739 | pass · 100 · 0.0309 | pass · 45 · 0.0107 | 4/4 | 4/4 | $0.1364 | 416 s | 739 s |
| baseline `20261015T071250Z` | pass · 65 · ≥ 0.0230 | pass · 155 · ≥ 0.0756 | pass · 115 · ≥ 0.0356 | pass · 35 · 0.0096 | 4/4 | 4/4 | ≥ $0.1438 | 370 s | 11.5 min |

`archon-bench smoke` reported PASS (Archon 4/4, baseline 4/4). The ledger trial
ran its implement stage three times, because the stage agent twice wrote its
commit hash with a typo (a space inside it, then a missing digit) and the
ship record refused it. The third run passed.

### Re-gate findings

Read from the runs' item captures, the sandbox Aether store (read-only) and
the Codex session records of the stage agents named below.

- **Next's third run** is the whole gap. In `fix-off-by-one` and
  `quality-retry-budget` the orchestrator committed the fix in its own
  repository mirror and queued nothing (zero items), so the graded checkout is
  untouched. v1.3.1's orchestrator did the same on `quality-retry-budget` in
  the first gate. The orchestrator's prompt is unchanged since v1.3.1; the
  one change to `pump/orchestrator.rs` is the #144 reply tracker. Both arms'
  `review-seeded-bug` answers name the empty-signature `startsWith` bug
  without the rubric's forgery/bypass words: the lexical miss of the first
  gate.
- **"Completion tools not offered" is a search miss by the agent.** Both
  next-run-1 implement agents that reported missing tools
  (`agt_01M59XH9RYJGKHN32N4KRTR94P`, `agt_01M59XWEW16ACFVC54JDPE6X25`) used
  Codex's code-mode `exec` to search `ALL_TOOLS` with a filter that cannot
  match Aether's own tools: `/archon.*(complete_task|delegate|wait_for|fail_task)/`
  over name and description, then `/mcp__aether__mcp__archon__/` over names.
  The first gate's ledger agent `agt_01M59E1ABCGFM4WFXW0N3QTX7K` searched
  `/archon/` over names. `mcp__aether__complete_task` and its description
  contain no "archon". The Codex record does not list the offered tools, so
  their presence is not proven, but the agents' conclusion rests on a search
  that would have missed them. This replaces the first gate's suspected
  Aether-side event (aether #221). Both items passed on their second run.
- **Unreadable reviewer verdicts remain, on both builds.** Three implement
  runs failed because a reviewer ended without calling `complete_task`. Next
  run 2 (ledger) and v1.3.1 run 2 (`fix-off-by-one`) had a prose
  "**Verdict: pass.**" summary. Next run 3 (ledger) had a JSON verdict
  followed by prose. `74a33e3a` reads a whole written-out `complete_task`
  call; it still treats prose, and JSON followed by prose, as no verdict.
  Each item passed on its second run.
- Extra implement runs: next 4 over three runs, v1.3.1 1. Next's +15%
  median cost includes them.

## First gate (2026-10-19, at `600ddae3`)

Verdict then: **REGRESSION (Luna).** On `codex/gpt-6-luna`, next loses
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

### Versions

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

### Runs

Each cell: pass · trial wall s · cost $ (billed plus Aether's estimate; `≥`
marks an Archon trial whose cost Aether reported as incomplete).

#### Haiku 5.5

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

#### Luna (`codex/gpt-6-luna`)

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

### Next against v1.3.1 and the baselines

| Model | Check | next | v1.3.1 | baseline | Result |
|---|---|---|---|---|---|
| Haiku | Archon passes | 4/4, 4/4 | 4/4 | 4/4 | kept |
| Haiku | Archon cost | $0.1015, $0.1049 | $0.1716 | $0.1438 | −41% vs v1.3.1, −29% vs baseline |
| Haiku | Archon trial wall | 326 s, 326 s | 461 s | 370 s | −29% vs v1.3.1, −12% vs baseline |
| Luna | Archon passes | 2/4, 3/4 | 3/4 | 4/4 | **review-seeded-bug lost** (both next runs); ledger lost in the gate run |
| Luna | Archon cost | $0.0755, $0.0667 | $0.0438 | $0.0760 | +72% / +52% vs v1.3.1; −1% / −12% vs baseline |
| Luna | Archon trial wall | 681 s, 496 s | 316 s | 737 s | +115% / +57% vs v1.3.1; −8% / −33% vs baseline |

### The Luna failures

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

### Findings

Read from the code (`v1.3.1..50076851`) and from the gate's sandbox store
(`archon-bench/r140-062029/data/aether/aether.db`, read-only); no benchmark
was rerun. Everything a stage or orchestrator agent sees is unchanged since
v1.3.1: the orchestrator and topic prompts, the pipelines, the stage contract
(`archon-pipeline/src/contract*`), `stage_create_agent`, the hidden tools,
and every item's model plan (`codex/gpt-6-luna`, medium, balanced resolved
to fast, `host_context` full in all three Luna runs). The one wording change
is the `request_authority` `quote` description, a tool none of these runs
called.

Ruled out:

- **Protocol 18 → 24.** On aether `90aff1fa` the negotiated version only
  changes frame encoding: image refs before 19, accounting deltas from 20
  (Archon subscribes with `deltas: false`), `not_started` replay frames from
  23, and the `NoModel` error code from 21. No agent setting depends on it.
- **#160.** It records and compares `provider/model effort`; it reports
  drift and selects nothing.
- **#144.** The reply tracker changes only when a second operator message
  arrives mid-turn; the bench sends one.
- **Aether #206, #216, #195.** #206 refuses side-effect calls batched after a
  park, with a `TURN_IS_OVER` tool error; the failed stage agent has no
  failed tool call. #216 changes description wording only. #195 is a store
  query change. In any case the same aetherd served the v1.3.1 runs.

#### `review-seeded-bug`: lexical miss, no Archon cause

All three Luna answers find the same bug with the empty-signature input
(`body.`). v1.3.1's says "authentication bypass"; next's say "accepts any
prefix … including an empty signature" and "passes the signature check". The
first next answer's only "bypass" describes another finding, 416 characters
from the nearest prefix anchor, just outside the rubric's 400-character
window. This is wording variance at one trial per cell. The same miss was
recorded with an older Archon in `luna-no-baseline-20261015`.

#### `quality-ledger-transfer` (gate run): the agent never tried the tools

Stage agent `agt_01M59E1ABCGFM4WFXW0N3QTX7K` called only `bash`,
`apply_patch` and `mcp__aether__mcp__archon__message_orchestrator`. It never
attempted a harness tool, then said they were absent. In the same next runs,
seven other Luna implement agents created with the same request called
`mcp__aether__complete_task` through the same bridge. Six of them also called
`delegate` and `wait_for`; the seventh ran an item with review `none`. Aether's list for an engine agent (`engine_access.rs`
`engine_tool_specs`) depends only on its exposure, scope and position, so
nothing in Archon or in the agent's state would withhold them.

**Aether finding** (aether `90aff1fa`): the record cannot show what Codex
offered that turn.

- `crates/aether-engine/src/codex_app/driver.rs:73-98` (`mcp_inventory`)
  starts the turn once the `aether` server lists any tools at all; it does
  not check that `complete_task` is among them.
- The inventory ("Aether MCP tools ready: …") is logged only at debug, in
  `crates/aether-runtime/src/turn_engine/events.rs:679-681`.
- `engine.session` records `"tools": []` for every Codex session
  (`codex_app/driver.rs:417-421`).

If Aether recorded the inventory as an event, and refused to start a
task-holding agent's turn without `complete_task`, the next occurrence would
either be proven or prevented.

#### Cost and wall time: unreadable reviewer verdicts, now fixed

Next ×2 cost $0.0229 more than v1.3.1. Of that:

- $0.0145 is `quality-retry-budget`: v1.3.1's orchestrator did the work in
  its own mirror without queueing and failed in 40 s for $0.0048, while next
  queued, implemented and passed.
- $0.0092 is `quality-ledger-transfer`, where the implement stage ran twice.

In the gate run, `fix-off-by-one` needed a second item, and the orchestrator
chose `strict` review (three reviewers) for the ledger, where v1.3.1's chose
`standard`.

Both extra implement runs have one cause. Reviewers `agt_01M59DTPQ…` and
`agt_01M59FK4J…` passed the change, but wrote their `complete_task`
arguments, `{"summary": …, "structured": {"verdict": "pass", …}}`, as their
final text. Aether auto-completed them with that text as the summary, and
`Verdict::read_result` accepted only a bare verdict there, so archond failed
the stage with "its result carries no `verdict`". The reader is unchanged
since v1.3.1, whose Luna reviewers happened to call `complete_task`.
`read_result` now also reads the verdict inside a whole written-out
`complete_task` call. Explicit `structured` still wins, and prose is still
no verdict. The two captured summaries are a test fixture
(`crates/archon-pipeline/tests/fixtures/luna-wrapped-review-results.json`).
