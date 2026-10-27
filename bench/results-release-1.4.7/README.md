# Release 1.4.7 smoke gate

**Verdict: PASS, no decrease.** The release head is `f23dd517` (1.4.7, with
the five pre-release fixes). Archon passed 11 of 12 Haiku trials there, the
confirmation run included, and v1.4.6 passed 8 of 8. The miss was a lexical
grader miss on a correct review (see "The head's miss" below). The
confirmation run passed 4/4. Luna passed 4/4 on both builds. Both arms ran on
the released Aether 1.2.5.

`CARGO_BUILD_JOBS=4 make ci` passed on the same head: 3364 tests passed,
0 failed and 9 ignored.

**Cost: no increase.**

- The head's Haiku costs are complete. Over its three runs, Archon cost a
  mean of $0.1025 per run, against at least $0.1256 on v1.4.6.
- v1.4.6's run 1 chose `strict` review on `quality-ledger-transfer` (at
  least $0.0758; Aether reported that trial's cost as incomplete). Every
  other Haiku run chose `standard` there.
- Leaving out `quality-ledger-transfer`, the Haiku means were $0.0698 on the
  head (three runs) and $0.0697 on v1.4.6 (two runs).
- Luna's estimate was $0.0637 on the head against $0.0608 on v1.4.6. The gap
  is one trial: on `fix-off-by-one`, v1.4.6's orchestrator chose review
  `none` ($0.0102) and the head chose `standard` ($0.0192). Over the other
  three tasks the head's estimate was lower ($0.0445 against $0.0506). The
  earlier round showed the same pattern on `quality-retry-budget`.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 3/4 | $0.0929 | 290 s | 4/4 | FAIL · $0.13 |
| Haiku v1.4.6, run 1 | 4/4 | ≥ $0.1471 | 396 s | 4/4 | PASS · ≥ $0.19 |
| Haiku head, run 2 | 4/4 | $0.0984 | 315 s | 4/4 | PASS · $0.13 |
| Haiku v1.4.6, run 2 | 4/4 | $0.1040 | 456 s | 4/4 | PASS · $0.14 |
| Luna head | 4/4 | $0.0637 * | 390 s | 4/4 | PASS · $0.08 * |
| Luna v1.4.6 | 4/4 | $0.0608 * | 391 s | 3/4 | PASS · $0.07 * |
| Haiku head, run 3 (confirmation) | 4/4 | $0.1160 | 427 s | 4/4 | PASS · $0.15 |

- **Cost marks.** Haiku's costs are billed. Luna's (`*`) are Aether's
  estimates for a subscription engine, which bills $0, so they are not
  bounds. `≥` marks a cost that Aether reported as incomplete; the true cost
  is at least that: v1.4.6 run 1's `quality-ledger-transfer` trial.
- **Gate cost.** These seven runs cost ≥ $0.74 billed and $0.15 estimated.
- **Earlier round.** One earlier round of six runs ran on `148eec71`, before
  the last pre-release fix. It cost ≥ $0.64 billed and $0.15 estimated. Its
  results are under "Earlier round" below.

## The head's miss

Haiku head run 1 missed `review-seeded-bug` (score 0.5).

- **The review was correct.** The answer opens by calling the bug "one
  critical forgery bug". Its first finding is that `verify()` accepts a
  token with an empty signature, so anyone can mint a token.
- **Why the grader missed it.** The rubric needs a forgery word within
  400 characters of a prefix or empty-signature word.
  - The answer's second paragraph notes that the prompt's `session-lib` is
    registered as `review-seeded-bug`.
  - That note sits between "forgery" and "empty signature", so the two words
    are more than 400 characters apart.
  - Its later forgery wording ("mint a token", "needs no secret") is not in
    the rubric's list.
- **Not new on the head.** In the earlier round, all four Haiku answers on
  both builds also mentioned `session-lib`, and all passed. The 1.4.3 gate
  recorded a similar wording miss on this task.
- **Confirmation.** Haiku head run 3 passed `review-seeded-bug` and the
  other three tasks.
- **Totals.** The head passed 15 of 16 trials in this round, and the two
  rounds together 27 of 28. v1.4.6 passed 24 of 24.

## Method

The method is the 1.4.6 gate's:

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baselines `baseline.json` (Haiku) and `baseline-luna.json`
  (Luna).

The baselines, tasks and graders under `bench/` are byte-identical in v1.4.6
and on the head. In each round the runs alternated between the head and
v1.4.6, one at a time. The confirmation ran after the round.

- **Head arm.** `archon` and `archon-bench` were built from `f23dd517` with
  `cargo build --release -p archon -p archon-bench --locked`. The earlier
  round's were built from `148eec71` in the dev profile.
- **v1.4.6 arm.** The released `archon` 1.4.6 (`x86_64-unknown-linux-musl`
  archive, from `gh release download v1.4.6`). The archive passed both the
  release's `SHA256SUMS` and its own `.sha256` check. `archon-bench` was
  built from the `v1.4.6` tag.
- **Aether, both arms.** The released Aether 1.2.5
  (`x86_64-unknown-linux-musl` archive), which passed both its release's
  `SHA256SUMS` and its own `.sha256` check. It reports `aether 1.2.5`.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held copies (mode 600) of the engine credentials and the
    `archon-bench` git identity.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. One attempt at v1.4.6 run 1 was refused before
  creating its sandbox (54 minutes left). It ran after the host refreshed
  the token.

## Findings

- **Daemon logs.** The archond logs of both builds show only the usual
  tightening of the sandbox's socket directory and, on runs whose agents'
  usage was revised down, "usage total went down; keeping what was already
  charged" (head: 1 Luna line; v1.4.6: 1 Haiku and 2 Luna lines). That
  warning dates from 1.0.0. No other warning or error appeared.
- **Aether protocol floor.** Archon 1.4.7 raises the lowest Aether protocol
  it accepts to 17. The released Aether 1.2.5 serves 17 to 24. Every run on
  both builds connected to it at protocol 24.
- **What reaches the agents.** Between v1.4.6 and the head:
  - the built-in prompts and the stage contract diff against
    `{{base_commit}}`, the fork point, instead of `{{base_branch}}`;
  - the orchestrator's prompt says the TUI's read commands are answered for
    it, and that it can schedule recurring work;
  - the orchestrator and stages no longer see Aether's native `notify`, and
    stages no longer see `refresh_provider_usage`.
  None of the smoke tasks ran a refresh or a gate answer.

## Earlier round

These runs used head `148eec71`, before the lessons refresh fix.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 4/4 | $0.1037 | 291 s | 4/4 | PASS · $0.14 |
| Haiku v1.4.6, run 1 | 4/4 | $0.1293 | 396 s | 4/4 | PASS · $0.16 |
| Haiku head, run 2 | 4/4 | ≥ $0.1405 | 446 s | 4/4 | PASS · ≥ $0.18 |
| Haiku v1.4.6, run 2 | 4/4 | $0.1201 | 356 s | 4/4 | PASS · $0.16 |
| Luna head | 4/4 | $0.0679 * | 422 s | 4/4 | PASS · $0.08 * |
| Luna v1.4.6 | 4/4 | $0.0602 * | 371 s | 4/4 | PASS · $0.07 * |

- **Cost comparison.** It is inconclusive. Head run 2's
  `quality-ledger-transfer` trial chose `strict` review, and Aether reported
  its cost as incomplete (at least $0.0662). So the head's Haiku mean is at
  least $0.1221, against $0.1247 on v1.4.6. $0.0052 more of missing usage
  would put the head above v1.4.6. That is why this round is not used for
  the cost verdict.
- **Other review choices.** v1.4.6 run 2 also chose `strict` there
  ($0.0562). On Luna, v1.4.6 chose review `none` for
  `quality-retry-budget`, and the head chose `standard`.
- **Logs.** The archond logs showed only the same two warnings. Every head
  run connected at protocol 24.

## Evidence

This item's artifact (`release-1.4.7-gate-evidence`) keeps:

- the results and logs of both rounds and the confirmation;
- the sandbox scripts (`scripts/gate.sh`, `scripts/run.sh`);
- the `make ci` logs;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`f23dd517`, release profile) | `6590c005` |
| head `archon-bench` (`f23dd517`, release profile) | `6415ff45` |
| earlier-round `archon` (`148eec71`, dev profile) | `4e0a3205` |
| earlier-round `archon-bench` (`148eec71`, dev profile) | `7236e403` |
| released v1.4.6 `archon` | `acdc7128` |
| `archon-bench` v1.4.6 tag | `2f9555b9` |
| released Aether 1.2.5 | `0c40c972` |
