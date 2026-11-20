# Trim after 1.6.4: stage turns and time on small diffs

**Verdict: back to v1.6.3 levels, with no correctness lost.** On the head
(`ff3b40d0`), Haiku and Luna both passed **4/4 on published v1.6.3 and 4/4 on
the head**. Summed stage turns and stage time are within 10% of v1.6.3 on
both models. Every `direct` lead delegated at most one reviewer, and no stage
recorded `missing_verdict_line`. This is a small smoke (one trial per task),
not a statistical pass.

| Arm | Pass | Archon time | Stage time | Stage turns | Reviewers | Reviewer turns |
|---|---:|---:|---:|---:|---:|---:|
| Haiku v1.6.3 | 4/4 | 371.2 s | 267.6 s | 96 | 4 | 38 |
| Haiku head | 4/4 | 375.7 s (+1%) | 260.2 s (-3%) | 103 (+7%) | 3 | 13 |
| Luna v1.6.3 | 4/4 | 631.0 s | 458.9 s | 61 | 4 | 36 |
| Luna head | 4/4 | 580.9 s (-8%) | 419.4 s (-9%) | 61 (0%) | 3 | 19 |

The 1.6.4 smoke measured, by the same method, Haiku at 90 → 113 stage turns
and 233 → 368 s of stage time, and Luna at 55 → 63 turns and 477 → 516 s.

## What cost the extra turns in 1.6.4

From the 1.6.4 evidence (`release-1.6.4-evidence`):

- **Luna: a second reviewer to confirm a pass.** The lead paraphrased the
  verdict JSON into the reviewer's instructions. The reviewer answered in
  prose (`complete_task: {...}` inside its summary, with a flat `risk_level`
  instead of `risk.level`), so the lead delegated another reviewer to get a
  verdict it could read. The contract now says to paste the JSON verbatim,
  puts the risk call inside it, and says a lead delegates no more reviewers
  than its review names, "not even to confirm a pass". The exceptions are the
  reads the round, coverage and replacement rules already require.
- **Haiku: legitimate delta reviews.** The extra Haiku reviewers in 1.6.4
  were delta reads after a blocking test-gap fail. The rules require these,
  so they stay. The Haiku difference was mostly run-to-run noise.
- **`missing_verdict_line` on `direct`.** This was a record only. The
  ungated `direct` implement stage owes no verdict, so the note is no longer
  stored for a stage its driver does not gate on a verdict.

Per lead, the head's reviewer counts were 1, 1, 1 (Haiku) and 1, 0, 1, 1
(Luna). v1.6.3's were 1, 2, 1 and 1, 1, 1, 1. On Luna
`quality-ledger-transfer`, the head's `direct` implement escalated on the
spec's concurrency, as in 1.6.4 (#440). The item went on to a `build` stage
that delegated one reviewer and passed. v1.6.3 also needed a second stage for
that item.

Turn counts move by several turns between identical runs. On Haiku, the head
spent fewer reviewer turns (13 against 38) and more lead turns. On Luna,
stage time fell 9% at equal turns. Read the ±10% band as noise, not as a
trend.

## Runs

One run at a time, alternating old and new, each in its own private
sandbox. A full run has four Archon tasks and four direct-Aether controls.
Times sum only the Archon trials.

| Run | Head | Load5 | Archon | Direct Aether | Archon time |
|---|---|---:|---:|---:|---:|
| old-haiku-1 | v1.6.3 | 3.51 | 4/4 | 4/4 | 371.2 s |
| new-haiku-1 | `36bc9156` build | 1.04 | 3/4 (void) | 4/4 | 280.5 s |
| new-haiku-2 | `ff3b40d0` | 3.06 | 4/4 | 4/4 | 375.7 s |
| old-luna-1 | v1.6.3 | 1.63 | 4/4 | 4/4 | 631.0 s |
| new-luna-1 | `ff3b40d0` | 0.78 | 4/4 | 4/4 | 580.9 s |

| Archon task | Haiku old | Haiku new | Luna old | Luna new |
|---|---:|---:|---:|---:|
| fix-off-by-one | 70.6 s | 65.2 s | 140.2 s | 95.2 s |
| quality-ledger-transfer | 160.3 s | 150.2 s | 275.4 s | 255.4 s |
| quality-retry-budget | 105.2 s | 130.2 s | 180.3 s | 185.3 s |
| review-seeded-bug | 35.1 s | 30.1 s | 35.1 s | 45.1 s |

**new-haiku-1 is void and not counted.** Its harness accepted a helper's
`agent.created` but still refused that helper's `task.created` ("event 1357
is not the orchestrator's"), so `review-seeded-bug` was not graded. The
harness now accepts every event of an agent the orchestrator created (its
`parent` is the orchestrator) and feeds only the orchestrator's own events
to the reply tracker. An event of any other agent is still refused.
new-haiku-2 reran the arm on that harness. Its other three tasks match
new-haiku-1 in outcome and in reviewers per lead.

## Method

- A turn is one assistant `agent.message` event of a stage agent
  (`archon-stage:*`), and its reviewers are counted apart from it. Stage time
  sums each item stage's `started_at..finished_at` in `archon-items.json`.
  This reproduces the 1.6.4 brief's 90 → 113 turns and 233 → 368 s from the
  1.6.4 evidence. The script is `measure.py` in the artifact.
- The head is built with `cargo build --release --locked -p archon -p
  archon-bench`. The old arm is the published `archon` v1.6.3 musl archive
  with `archon-bench` built from the `v1.6.3` tag, as in 1.6.4. Both arms
  use the published Aether v1.4.2 musl archive. Both archives match the
  hashes in `bench/results-release-1.6.4/README.md`.
- Haiku uses `claude-code/claude-haiku-5-5`, and Luna uses
  `codex/gpt-6-luna` with `bench/results-smoke/baseline-luna.json`. Each run
  uses medium effort, one trial per task, serial execution and the $7
  admission limit.
- The wrapper is 1.6.4's `smoke-run.py` with this item's paths. Each run
  started below load5 14, with its own HOME, config, data, temporary paths
  and sockets. Engine credentials were copied at mode 600, never printed,
  and removed with the sandbox. Each sandbox root was checked removed
  afterwards. The installed daemons and their configuration were not
  touched.

| Binary | SHA256 |
|---|---|
| Published `archon` 1.6.3 | `8fe4927e3bc2d0c66e590e9fab58de8aca468cdfdc1345b07d3903da62b94981` |
| Published `aether` 1.4.2 | `c0510fe9d0669cf8eda7cc3f3154db21eb62c297b24b30896244e65398ab9a88` |
| Head `archon` (`ff3b40d0`) | `ce0e00452467a350db85e224c69a4929eb4b96dcb1a5d8fc2395f91027f4e644` |
| Head `archon-bench` (`ff3b40d0`) | `b27acb1c834c88cb109b8868a875cc3463a5765a70d0b87dd1214b44929f6f02` |
| `archon-bench` from `v1.6.3` | `cd726bcca9030ecacfb3e8dab357ddbe8e9e486d60555516694c9ae269a29efa` |

The `v1.6.3` `archon-bench` hash differs from 1.6.4's because it is built
with this host's toolchain. Its source is the `v1.6.3` tag.
