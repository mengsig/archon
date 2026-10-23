# Release 1.4.5 smoke gate

**Verdict: PASS, no decrease.** On the release head `00c85da6` (1.4.5) a
confirming Haiku run passed 4/4, and the two gate runs scored 7/8 between
them. v1.4.4 scored 8/8 in its two runs. Neither head miss came from a change
in this release: one was a rubric wording miss and the other a model waiting
on the wrong task id. Luna passed 4/4 on both builds. The head cost less per
Haiku run than v1.4.4: a mean $0.1175 against $0.1370, or $0.1153 over all
three head runs. `CARGO_BUILD_JOBS=4 make ci` passed on the same head (3230
tests passed, 0 failed, 9 ignored).

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 3/4 | $0.1205 | 361 s | 4/4 | FAIL · $0.16 |
| Haiku v1.4.4, run 1 | 4/4 | $0.0967 | 300 s | 4/4 | PASS · $0.13 |
| Haiku head, run 2 | 4/4 | $0.1145 | 932 s | 4/4 | FAIL (timeout) · $0.15 |
| Haiku v1.4.4, run 2 | 4/4 | ≥ $0.1773 | 706 s | 4/4 | PASS · ≥ $0.22 |
| Luna head | 4/4 | $0.0688 * | 356 s | 4/4 | PASS · $0.08 * |
| Luna v1.4.4 | 4/4 | $0.0430 * | 271 s | 3/4 | PASS · $0.05 * |
| Haiku head, run 3 (confirmation) | 4/4 | $0.1108 | 361 s | 4/4 | PASS · $0.15 |

Haiku's costs are billed. Luna's (`*`) are Aether's estimates for a
subscription engine, which bills $0, so they are not bounds. `≥` marks a cost
that Aether reported as incomplete. The whole gate cost $0.81 billed and $0.13
estimated.

## Method

The method is the 1.4.3 gate's: one trial per task and arm (`archon`,
`aether`), medium effort, serial, the default $7 cap, and the stored
baselines `baseline.json` (Haiku) and `baseline-luna.json` (Luna). The
baselines, tasks and graders under `bench/` are byte-identical in v1.4.4 and
on the head. The runs alternated between head and v1.4.4, one at a time.

- **Head arm:** `archon` and `archon-bench` built from `00c85da6` with
  `cargo build -p archon -p archon-bench --locked` (dev profile). Aether was
  built from the head of the Aether 1.2.4 release pull request
  (mengsig/aether#288, `6d0099e`, dev profile), and the binary reports
  `aether 1.2.4`. Since v1.2.3 that Aether changes only its TUI (#286, #287)
  and its release records.
- **v1.4.4 arm:** the released `archon` 1.4.4 and Aether 1.2.3
  (`x86_64-unknown-linux-musl` archives). Each archive passed its release's
  `SHA256SUMS` and its own `.sha256` check. The v1.4.4 release is still a
  draft, and `gh release download` fetched its assets. `archon-bench` was
  built from the `v1.4.4` tag.
- **Sandbox:** each run had a new private directory under
  `~/.local/share/archon-bench/`. It held its own aetherd, Archon and Aether
  config and data, sockets, `TMPDIR` and `HOME`, and everything started
  through an `env -i` wrapper. The `HOME` held copies (mode 600) of the engine
  credentials and the `archon-bench` git identity. The aetherd was stopped by
  its socket and PID. After each run the whole sandbox, credentials included,
  was removed. No operator daemon, configuration or data was used.

## Findings

- **Haiku head run 1, `review-seeded-bug` (score 0.5).** The answer found the
  bug: `verify` accepts an empty or truncated signature because the check is
  `expected.startsWith(signature)`. It called the bug a "critical forgery bug"
  and said "anyone can mint an admin session without the key". The rubric
  wants the forgery or bypass wording close to the prefix finding. Here it
  was in the opening line and the first finding's last bullet, so the second
  check scored MISS. The 1.4.3 gate had the same wording miss on Luna. Every
  other `review-seeded-bug` answer in this gate passed.
- **Haiku head run 2, `quality-retry-budget` (timeout, graded pass).** The
  implement agent sent a reviewer at 21:30:38. It then called `wait_for` with
  `tasks` set to its own task id instead of the reviewer's, with a 600 s
  timeout. A task cannot finish while its owner waits on it, so the agent
  slept until the deadline (21:40:39). The reviewer had passed the change at
  21:31:08. The agent then completed its task, and the run delivered with a
  passing review. The work was correct, but the trial ran 721 s and hit the
  smoke's time limit. The mistake was the model's choice of id. Neither
  release changed the stage contract, the orchestrator prompt or Aether's
  `wait_for`. v1.4.4's run 2 had a long trial too: on `quality-ledger-transfer`
  the orchestrator chose `strict` review, and the trial took 486 s and
  ≥ $0.11.
- **Confirmation.** A third Haiku run on the head passed 4/4 at $0.1108 in
  361 s, with no trial over 150 s. So the head passed 11/12 Archon trials
  over three runs, and v1.4.4 passed 8/8 over two.
- **Cost.** Haiku's Archon arm cost a mean $0.1175 on the head over the two
  gate runs, against $0.1370 on v1.4.4. Leaving out v1.4.4's `strict`
  review run, each head run ($0.1108-0.1205) cost more than v1.4.4's run 1
  ($0.0967). Every head run still falls within the 1.4.3 gate's range for
  both builds ($0.0982-0.1280). Most of the gap is `quality-retry-budget`
  ($0.029-0.040 on the head against $0.022-0.025), where the review gate did
  extra work in two head runs. In run 1 the reviewer read a commit the run's
  workspace no longer held, so the automatic repair retry (#237, since 1.4.3)
  ran a second implement attempt. In run 3 the agent fixed a reviewer's
  finding after the review, so a second reviewer read that change. Both
  follow from the model's choices under gate rules that v1.4.4 already had.
  Luna's estimate was higher on the head ($0.0688 against $0.0430). On
  `fix-off-by-one` the head's orchestrator chose `standard` review where
  v1.4.4 chose none. That reviewer's first result had no readable verdict, so
  the repair retry ran once. Review levels are the orchestrator's choice, and
  its prompt did not change.
- **What reaches the agents.** Between v1.4.4 and the head, only the display
  names of stage agents under a run node changed (#248). Prompts, the stage
  contract and the pipelines did not change.

## Evidence

The results, logs, sandbox scripts (`scripts/gate.sh`, `scripts/run.sh`),
the `make ci` log and the sha256 of every binary are kept as this item's
artifact (`release-1.4.5-gate-evidence`). Binary sha256 prefixes: head
`archon` `976b82bd`, released v1.4.4 `archon` `2754e910`, Aether 1.2.4 PR
head `9d9b0596`, released Aether 1.2.3 `d2b03448`.
