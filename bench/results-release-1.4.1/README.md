# Release 1.4.1 smoke gate

**Latest: no significant decrease** (Haiku, all gates pooled: Archon 40/44 on
1.4.1 against 40/40 on v1.4.0, Fisher p=0.12); see
[the last section](#larger-haiku-rerun-on-the-release-head).

**Verdict: PASS WITH ONE UNREPRODUCED MISS.** At `70aebe96` (1.4.1) the
first Haiku run of next passed 3/4 Archon trials against a 4/4 baseline, and
`archon-bench smoke` reported FAIL. Its repeat passed 4/4. v1.4.0 passed 4/4
on Haiku and Luna alike. Luna on next passed 4/4. No change between v1.4.0
and 1.4.1 touches the orchestrator prompts. A reader holding the rule "if
performance decreased, report it and stop" should treat the Haiku miss as a
reported, unreproduced regression signal. `make ci` is green (2978 tests, 0
failed).

## Method

One trial per task and arm (`archon`, `aether`), medium effort, serial, a $7
cap, each run in its own new private sandbox under `archon-bench/` with an
`env -i` environment, and the stored baselines `baseline.json` (Haiku) and
`baseline-luna.json`. Each sandbox ran its own aetherd, built from
mengsig/aether `96cfaad0` (1.2.1), and was stopped after its run. Next was
`archon` 1.4.1 at `70aebe96`; the baseline arm was `archon` built from
`v1.4.0`. No operator daemon was used.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku next, run 1 | 3/4 | $0.0929 | 300 s | 4/4 | FAIL |
| Haiku next, run 2 | 4/4 | $0.1253 | 391 s | 4/4 | PASS |
| Haiku v1.4.0 | 4/4 | $0.0977 | 375 s | 4/4 | PASS |
| Luna next | 4/4 | see below | 459 s | 4/4 | PASS |
| Luna v1.4.0 | 4/4 | see below | 521 s | 3/4 | PASS |

Luna's Archon cost is an Aether estimate (`cost_gap` mixed); the smoke totals
for the whole run, both arms, were $0.07 for next and $0.08 for v1.4.0.

## Findings

- **The Haiku miss** is `review-seeded-bug` in run 1. The orchestrator
  answered that it could not review `src/tokens.js` because no `session-lib`
  repository was registered, and declined to read the one repository there,
  `review-seeded-bug`. It queued no item (15 s, $0.0058). The same task
  passed in run 2 and on v1.4.0. That is the model's refusal, not an Archon
  failure to start or deliver work; one sample cannot show whether 1.4.1 makes
  it likelier.
- **Cost.** Haiku run 1 cost less than v1.4.0 ($0.0929 against $0.0977), though
  its review trial did no work. Run 2 cost +28% ($0.1253), almost all from one trial
  (`quality-ledger-transfer`, $0.06 against $0.03). One trial per cell cannot
  separate that from noise.
- **Luna** passes all four tasks on next, at a lower wall time and smoke total
  than v1.4.0.
- The v1.4.0 Luna `aether` arm missed `review-seeded-bug`, the lexical miss
  the 1.4.0 gate described; it does not gate.

Run results are kept as this item's artifact (`release-1.4.1-gate-evidence`).

## Rerun: is the Haiku miss a flake?

**Verdict: flake.** Three more Haiku smoke runs of this branch passed 4/4
Archon trials each, as did three more of v1.4.0. Across all runs the miss
count is 1/20 Archon trials on next (5 runs) against 0/16 on v1.4.0 (4 runs).
The `review-seeded-bug` failure did not recur in 6 further trials, so there
is no transcript to read for it; a single refusal in 20 trials does not
separate 1.4.1 from v1.4.0.

Same method as above: the same Aether 1.2.1 (`96cfaad0`) and the same
`archon` binaries, a new private `env -i` sandbox and aetherd per run, one run
at a time, medium effort, the Haiku baseline, no operator daemon. Runs were
interleaved (next, v1.4.0, next, ...).

| Run | Archon | Aether | Smoke total |
|---|---|---|---|
| Haiku next, rerun 1 | 4/4 | 4/4 | PASS · $0.14 |
| Haiku v1.4.0, rerun 1 | 4/4 | 4/4 | PASS · $0.14 |
| Haiku next, rerun 2 | 4/4 | 4/4 | PASS · $0.13 |
| Haiku v1.4.0, rerun 2 | 4/4 | 4/4 | PASS · $0.17 |
| Haiku next, rerun 3 | 4/4 | 4/4 | PASS · ≥ $0.16 |
| Haiku v1.4.0, rerun 3 | 4/4 | 4/4 | PASS · $0.13 |

`review-seeded-bug` on Archon took 30-50 s and cost about $0.01 in all six
runs. Between v1.4.0 and this branch no file with prompt, orchestrator,
repository or mirror in its path changed (`git diff --name-only v1.4.0..HEAD`),
which agrees with the earlier finding that the orchestrator prompts are
unchanged. The rerun artifacts were not kept; the sandboxes were removed.

## Gate on the final release head: a Haiku decrease

**Verdict: decrease, so the release stops here.** Haiku passed 7/8 Archon
trials on the release head against 8/8 on v1.4.0, and `archon-bench smoke`
reported FAIL for one of the head's two runs. Luna passed 4/4 on both. No
further reruns were made; whether 1.4.1 ships is the operator's call.

The head measured is `0238604b`: 1.4.1 with #182 (image paste, daemon
protocol 18), `serde_json` 1.0.154, `tokio` 1.53.3 and CI's
`taiki-e/install-action` 2.87.28. Later commits change only this file.
`CARGO_BUILD_JOBS=4 make ci` passed on that head (3012 tests passed, 0
failed, 9 ignored).

Same method as above, with one difference: each build ran under its own
harness. The head's `archon-bench` speaks daemon protocol 18, and v1.4.0's
archond speaks 17 and refuses it, so the v1.4.0 arm used `archon-bench`
built from `v1.4.0` (its tasks and baselines are byte-identical to the
head's). A first v1.4.0 attempt under the head's harness failed every
Archon trial on that refusal before any work. It is kept as evidence and
not counted. Aether was 1.2.1 at `96cfaad0`, the head of mengsig/aether
#233 when the runs began. The `archon` built from `v1.4.0` is byte-identical
to the earlier gate's (sha256 `44d29f95`). Runs were interleaved, one at a
time, each in a new private `env -i` sandbox with its own aetherd, all
removed afterwards.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 4/4 | $0.0905 | 302 s | 4/4 | PASS · $0.12 |
| Haiku v1.4.0, run 1 | 4/4 | $0.0910 | 326 s | 4/4 | PASS · $0.12 |
| Haiku head, run 2 | 3/4 | $0.0997 | 1427 s | 4/4 | FAIL (timeout) |
| Haiku v1.4.0, run 2 | 4/4 | $0.0951 | 326 s | 4/4 | PASS · $0.13 |
| Luna head | 4/4 | $0.0647 | 366 s | 4/4 | PASS · $0.08 |
| Luna v1.4.0 | 4/4 | $0.0630 | 371 s | 4/4 | PASS · $0.08 |

Haiku's Archon costs are floors (`cost_gap = interrupted`). Luna's mix
incomplete reported usage with Aether's estimates (`*` in the smoke output),
so they are not bounds.

### Findings

- **The miss** is `quality-ledger-transfer` in Haiku run 2 on the head. It
  hit the 20-minute trial limit at 1202 s. The branch it left passes the
  hidden grader ("graded on timeout: pass"), but a timeout never counts as a
  pass. The `direct` implement agent wrote and committed the change in under
  a minute, delegated its reviewer at 15:46:09, and called `wait_for` with
  only `message_from` (its parent) and `timeout_seconds: 900`, without the
  reviewer's task. The reviewer passed in its first turn, but the waiter woke
  only at 16:01:09, when the 900 s ran out. It then committed a small test
  fix, delegated a delta reviewer and made the same call again, and the
  trial ran out. Its turns are in the evidence
  (`haiku-next-2/replay-stage.txt`).
- **What changed for that agent.** No `wait_for` guidance changed between
  v1.4.0 and this head. The record stage's contract
  (`crates/archon-pipeline/src/contract.rs`) gained #180's "Risk level"
  section, so its prompt is longer and asks reviewers for a risk call. The
  earlier sections' "orchestrator prompts are unchanged" holds for the
  orchestrator only, not for this stage contract. One trial cannot show
  whether the longer contract made the mistake likelier.
- **Over both gates**, Haiku on 1.4.1 missed 2 of 28 Archon trials, each a
  different model mistake (a refusal, then this wait). v1.4.0 missed 0 of 24.
- **Cost and wall** of the passing runs match v1.4.0 within a few percent on
  both models.

The results, logs and scripts are kept as this item's artifact
(`release-1.4.1-gate2-evidence`).

## Larger Haiku rerun on the release head

**Verdict: no significant decrease: 40/44 vs 40/40, p=0.12** (Fisher's exact
test, two-sided; one-sided p=0.07). Four more Haiku smoke runs of the head
passed 14/16 Archon trials, and four more of v1.4.0 passed 16/16. The head's
misses are still not zero against v1.4.0's zero, and nothing in v1.4.0..HEAD
explains them, so the data cannot show a decrease but cannot rule out a small
one either.

The head is `2db9e061` (it differs from `0238604b` only in this README). Same
method as the last gate: the head ran under its own `archon-bench`, v1.4.0
under `archon-bench` built from `v1.4.0`, both archons built with
`CARGO_BUILD_JOBS=4`, Aether 1.2.1 from the v1.2.1 release (sha256 checked
against its `SHA256SUMS`), one run at a time, alternating builds, each in a new
private `env -i` sandbox with its own aetherd, removed afterwards.

| Run | Archon | Aether | Smoke total |
|---|---|---|---|
| Haiku head, run 1 | 4/4 | 4/4 | PASS · $0.18 |
| Haiku v1.4.0, run 1 | 4/4 | 4/4 | PASS · $0.20 |
| Haiku head, run 2 | 3/4 | 4/4 | FAIL · $0.16 |
| Haiku v1.4.0, run 2 | 4/4 | 4/4 | PASS · $0.17 |
| Haiku head, run 3 | 3/4 | 4/4 | FAIL · $0.14 |
| Haiku v1.4.0, run 3 | 4/4 | 4/4 | PASS · $0.22 |
| Haiku head, run 4 | 4/4 | 4/4 | PASS · $0.14 |
| Haiku v1.4.0, run 4 | 4/4 | 4/4 | PASS · $0.13 |

### Pooled over every gate

| | 1.4.1 | v1.4.0 | Fisher p |
|---|---|---|---|
| All Archon trials | 40/44 (4 missed) | 40/40 | 0.12 |
| Earlier gates only | 26/28 | 24/24 | 0.49 |
| This rerun only | 14/16 | 16/16 | 0.48 |
| `review-seeded-bug` only | 8/11 | 10/10 | 0.21 |

The pool counts the first gate (5 head and 4 v1.4.0 runs), the final-head gate
(2 and 2) and this rerun (4 and 4), without the uncounted v1.4.0 attempt under
the wrong harness.

### The misses

Both new misses are `review-seeded-bug`, the task whose Archon arm is
answered by the orchestrator alone (pipeline `ask`, 20-35 s).

- **Run 3: a refusal, the same as the first gate's miss.** The orchestrator
  answered that it could not find the `session-lib` repository (`list_repos`
  showed only `review-seeded-bug`), did not read it, and asked for a path.
  Score 0.0.
- **Run 2: a partial review.** The orchestrator read the repository (it noted
  that `package.json` names it `session-lib`), found the empty-signature and
  prefix-match bugs in `verify()` with working inputs, and scored 0.5: its
  answer says "anyone can mint any claims" but never uses a forgery or bypass
  word the rubric's second group matches. That is a grader-wording miss on a
  correct finding, not a failure to find the bug.
- No `quality-ledger-transfer` trial hit the timeout again: its eight head
  trials took 110-220 s. The `wait_for` mistake of the last gate did not recur.

### Does anything in v1.4.0..HEAD explain them?

No. The diff was read for the text each agent sees, not only for file names.

- **Orchestrator.** `crates/archon-daemon/prompts/` (the orchestrator prompt,
  its GitHub section and the topics) and `pipelines/` are byte-identical to
  v1.4.0. The tools the orchestrator calls changed in one place: `list_queue`
  prints `, risk <level>` for an item that has one, which `review-seeded-bug`
  never has (it queues nothing). `list_repos` and the repository registration
  are untouched. #182 adds an `images` field to the operator's message and the
  feed; with no image it sends the same text as before, so the orchestrator's
  first message is unchanged. #178 (notification center) only posts to Aether
  and changes no message to an agent. #181 (steer delivery) concerns stage
  runs, not the orchestrator.
- **Stage agents.** The one change to what a stage agent reads is #180's
  "Risk level" section in the record stage's contract (and a shorter one for
  a review stage after it), in `crates/archon-pipeline/src/contract.rs`. It
  adds the risk rubric, `risk` to the ship record and the reviewer's
  `risk` call. #179 makes the verdict check case-insensitive in code only.
  The delegation section, where `wait_for` is described ("Delegate with
  `wait: false`, then use `wait_for` with `any: true`", "Wait for every child
  (`wait_for`) or cancel it"), has no changed line. So the last gate's
  `wait_for` call, which passed `message_from` and no task, does not trace
  to any changed text; the only added text on that stage is the risk section
  above it.
- **Models and Aether** are the same on both arms (Haiku, Aether 1.2.1).

The refusal therefore looks like the model's sampling on a task whose prompt
names `session-lib` while the repository is registered as
`review-seeded-bug` (the harness's name for the fixture). Across all gates it
appears on the head in 2 of 11 `review-seeded-bug` trials and on v1.4.0 in 0
of 10; counting the partial review as well, 3 of 11 against 0 of 10 (p=0.21).
With so few samples that is within chance, but it is the one place where the
head is consistently behind.

### Limits

- 4 misses in 44 against none in 40 cannot separate a true rate of a few
  percent from none, so a small decrease is not ruled out.
- Costs of the passing runs are within noise of v1.4.0 (smoke totals $0.62
  for the four head runs, $0.72 for the four v1.4.0 runs). Haiku's costs are
  floors where Aether reports `cost_gap = interrupted`.
- Run results, scripts and the binaries' sha256 are kept as this item's
  artifact (`release-1.4.1-gate3-evidence`). Whether 1.4.1 ships stays the
  operator's call.
