# Release 1.4.11 smoke gate

**Verdict: FAIL again on a confirmed Luna cost increase; Haiku passes.**
The gate re-ran on `157fe387`, the release with #369 merged from `next`, on
Aether 1.2.10. Every trial passed on both models. #369 did not bring
Luna's cost back:

- Luna's Archon cost per run averaged $0.0956 over three head runs, the
  third a confirmation. v1.4.9 averaged $0.0649 over two runs, so the head
  costs +47%, as in the first attempt (+48%).
- Luna's wall time averaged 552 s against 378 s, +46%.
- By the release rule the gate stops here for the operator's decision.

Haiku cost less on the head: $0.1307 per run against $0.1851 on v1.4.10,
-29%, with wall time 390 s against 540 s and no repairs.

The baselines are the first attempt's runs, not re-run: v1.4.9 on Aether
1.2.8 for Luna and v1.4.10 on Aether 1.2.9 for Haiku, the releases each was
released with.

| Model | Runs | Trials passed | Archon cost/run | Wall/run | Repairs/run | Review cost/run |
|---|---|---|---|---|---|---|
| Luna, head `157fe387` | 3 | 12/12 | $0.0956 | 552 s | 2.3 (3, 2, 2) | $0.0234 |
| Luna, v1.4.9 | 2 | 8/8 | $0.0649 | 378 s | 1.5 (2, 1) | $0.0112 |
| Luna, first attempt `1b8176da` | 3 | 12/12 | $0.0960 | 579 s | 3.3 (3, 2, 5) | $0.0215 |
| Haiku, head `157fe387` | 2 | 8/8 | $0.1307 | 390 s | 0 (0, 0) | $0.0360 |
| Haiku, v1.4.10 | 2 | 8/8 | $0.1851 | 540 s | 1.5 (2, 1) | $0.0587 |
| Haiku, first attempt `1b8176da` | 2 | 8/8 | ≥ $0.1485 | 448 s | 0.5 (1, 0) | $0.0481 |

## Luna

- **Pass rate.** The head passed 12 of 12 trials, v1.4.9 8 of 8.
- **Confirmation.** Head runs 1 and 2 each cost more than both v1.4.9
  runs: $0.1044 and $0.0923. The third run confirmed it at $0.0901, still
  above both.
- **What #369 fixed.** None of the head's 7 repairs is the refusal #369
  targets: a `VERDICT:` line with no `head:` line, or a JSON verdict with
  prose after it. Those caused 6 of the first attempt's 10 repairs. Repairs
  fell from 10 to 7 in three runs, but cost did not fall.
- **What the 7 repairs were.** Each refusal fails the stage, and Archon
  repairs it with a new lead and new reviewers.
  - 3 reviewer results in shapes the gate does not read:
    - a YAML-style block, `complete_task:` then `verdict: approve`;
    - a `verdict` of `approve_with_nonblocking_finding`;
    - a JSON verdict with `complete_task ` before its `{` on the first line,
      so the object does not open the summary on a line of its own.
  - 2 leads named 1 reviewer where `strict` review needs 3. In runs 1 and
    2 the orchestrator gave `quality-ledger-transfer` review `strict` on
    the `direct` pipeline, which declares one reviewer. After the refusal
    it switched the item to `adaptive`. No earlier Luna run, on either
    release, chose `strict`. #369 changed only the stage contract, not
    what the orchestrator reads, so this is its own variance.
  - 1 lead result with no `review` object.
  - 1 genuine fail: a reviewer's blocking finding (a missing regression
    test) failed the lead's stage.
- **Cost of the repairs.** The runs that did not ship cost $0.0426,
  $0.0349 and $0.0285 on the head, against $0.0209 and $0.0045 on v1.4.9.
  Without them, Archon cost a mean of $0.0603 on the head and $0.0522 on
  v1.4.9 (+16%).
- **Strict review.** The two `strict` items cost $0.0529 and $0.0573,
  against $0.0198 to $0.0285 for the same task at `standard` on v1.4.9
  and in head run 3. On the
  tasks every Luna run reviewed at `standard` (`fix-off-by-one`,
  `quality-retry-budget`, `review-seeded-bug`), the head cost a mean of
  $0.0493 per run against $0.0409 on v1.4.9 (+21%).
- **Reviewer output.** 11 of 28 head reviewers set `structured`, 13 wrote a
  `VERDICT:` line and 5 a `head:` line. On v1.4.9, 2 of 8 set `structured`.

## Haiku

- **Pass rate.** Both releases passed 8 of 8 trials.
- **Cost and wall time.** Both were lower on the head in each run, as
  above. No run needed confirming.
- **Repairs.** None on the head, against 2 and 1 on v1.4.10.
- **Review.** Run 2 reviewed `quality-ledger-transfer` at `strict`, which
  cost $0.0646 against $0.0342 at `standard` in run 1.

## Aether arm

Every run passed 4/4 but Luna head run 1, which failed
`review-seeded-bug` (3/4). The Aether arm runs no Archon code. It cost
$0.035 to $0.037 per Haiku run and $0.012 to $0.014 (estimated) per Luna
run.

## Runs

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Repairs (cost) | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|---|
| Luna head, run 1 | 1.2.10 | 4/4 | $0.1044 * | 555 s | 12 ($0.0301) | 3 ($0.0426) | 3/4 | PASS · $0.12 * |
| Luna head, run 2 | 1.2.10 | 4/4 | $0.0923 * | 480 s | 10 ($0.0232) | 2 ($0.0349) | 4/4 | PASS · $0.10 * |
| Luna head, run 3 (confirmation) | 1.2.10 | 4/4 | $0.0901 * | 621 s | 6 ($0.0170) | 2 ($0.0285) | 4/4 | PASS · $0.10 * |
| Haiku head, run 1 | 1.2.10 | 4/4 | $0.1194 | 385 s | 3 ($0.0244) | 0 | 4/4 | PASS · $0.16 |
| Haiku head, run 2 | 1.2.10 | 4/4 | $0.1419 | 395 s | 5 ($0.0476) | 0 | 4/4 | PASS · $0.18 |

- Luna costs (`*`) are Aether's estimates: Codex usage is unbilled. Haiku
  costs are billed.
- Reviewer costs are summed from each run's Aether data, over every agent
  with role `reviewer`.
- A repair is an Archon run of an item that did not ship. Its cost is
  already in the Archon cost.
- **Gate cost.** The five runs cost $0.34 billed (Haiku) and about $0.32
  estimated (Luna).

Archon's cost and wall time per task, with the review level the
orchestrator chose (`standard` unless marked):

| Task | Luna head 1 | Luna head 2 | Luna head 3 | Haiku head 1 | Haiku head 2 |
|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0197 · 110 s | $0.0113 · 70 s | $0.0310 · 220 s | $0.0337 · 80 s | $0.0273 · 85 s |
| `quality-ledger-transfer` | $0.0529 · 255 s strict | $0.0573 · 270 s strict | $0.0285 · 210 s | $0.0342 · 115 s | $0.0646 · 125 s strict |
| `quality-retry-budget` | $0.0277 · 170 s | $0.0196 · 115 s | $0.0271 · 165 s | $0.0400 · 145 s | $0.0381 · 135 s |
| `review-seeded-bug` | $0.0041 · 20 s | $0.0040 · 25 s | $0.0035 · 25 s | $0.0115 · 45 s | $0.0119 · 50 s |

The baselines' per-task numbers are in the first attempt's record below
and in its evidence.

## First attempt (`1b8176da`, before #369)

The first gate ran on `1b8176da`, the release before #369, and failed on
the same Luna cost increase:

- **Luna.** $0.0839, $0.0816 and $0.1226 (confirmation) per run, a mean
  of $0.0960 against $0.0649 on v1.4.9 (+48%). Wall time was 500, 535 and
  701 s, against 415 and 340 s (+53%). Repairs were 3, 2 and 5. Of the 10
  refusals, 4 were a `VERDICT:` line with no `head:` line, 2 a verdict JSON
  with prose after it, 3 a pass taken back by later text and 1 a named
  fail no pass answered. #369 was written from these.
- **Haiku.** ≥ $0.1942 and $0.1028 per run, a mean of $0.1485 against
  $0.1851 on v1.4.10 (-20%), with wall time 448 s against 540 s and 1 and
  0 repairs.
- **Baselines.** These are the baselines this gate reuses:

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Repairs (cost) | Aether arm |
|---|---|---|---|---|---|---|---|
| Haiku v1.4.10, run 1 | 1.2.9 | 4/4 | $0.2068 | 600 s | 8 ($0.0657) | 2 ($0.0553) | 4/4 |
| Haiku v1.4.10, run 2 | 1.2.9 | 4/4 | $0.1633 | 480 s | 6 ($0.0517) | 1 ($0.0282) | 4/4 |
| Luna v1.4.9, run 1 | 1.2.8 | 4/4 | $0.0715 * | 415 s | 5 ($0.0124) | 2 ($0.0209) | 4/4 |
| Luna v1.4.9, run 2 | 1.2.8 | 4/4 | $0.0582 * | 340 s | 3 ($0.0100) | 1 ($0.0045) | 4/4 |

| Task | Haiku v1.4.10 1 | Haiku v1.4.10 2 | Luna v1.4.9 1 | Luna v1.4.9 2 |
|---|---|---|---|---|
| `fix-off-by-one` | $0.0539 · 125 s | $0.0271 · 80 s | $0.0224 · 130 s | $0.0177 · 105 s |
| `quality-ledger-transfer` | $0.0785 · 255 s strict | $0.0682 · 190 s strict | $0.0282 · 175 s | $0.0198 · 110 s |
| `quality-retry-budget` | $0.0654 · 190 s | $0.0585 · 180 s | $0.0160 · 85 s | $0.0174 · 110 s |
| `review-seeded-bug` | $0.0091 · 30 s | $0.0095 · 30 s | $0.0050 · 25 s | $0.0033 · 15 s |

The first attempt's full record, runs and logs are in item
`qi_01M6DHT8GH8XE3NV0ZQRRSFSJK`'s artifact `release-1.4.11-gate-evidence`.

## Aether 1.2.10

The head arm ran on the published Aether 1.2.10 `x86_64-unknown-linux-musl`
archive, downloaded again for this gate with `gh release download`. It
passed both the release's `SHA256SUMS` and its own `.sha256` check, its
`SHA256SUMS` matches the first attempt's, and it reports `aether 1.2.10`.
It serves protocols 17 to 25, as 1.2.9 does, and keeps store schema 28.
All 20 head trials connected at protocol 24, so Archon's
`PROTOCOL_VERSION` (24) and `MIN_PROTOCOL_VERSION` (17) stay as they are.

## Method

The method is the first attempt's, with the same scripts (`gate.sh`,
`run.sh`) and the same stored baselines `baseline.json` (Haiku) and
`baseline-luna.json` (Luna, `codex/gpt-6-luna`):

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the runs went one at a time, in the table's order, with the host's
  1-minute load below 10 before each.

`bench/` on `157fe387` differs from `1b8176da` only by the first attempt's
record.

- **Head arm.** `archon` and `archon-bench` were built from a `git archive`
  of `157fe387` with `cargo build --release -p archon -p archon-bench
  --locked`. The commit after it changes only this file.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held copies (mode 600) of the engine credentials (Claude
    and, for the Luna runs, Codex) and the `archon-bench` git identity.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. No run was refused.

## Findings

- **Luna reviewers still write verdicts the gate does not read.** #369
  removed the two shapes the first attempt found, but three new ones
  appeared: a YAML-style `verdict: approve`, the verdict word
  `approve_with_nonblocking_finding`, and `complete_task {…}` with a word
  before the JSON. Each costs a repair. A follow-up could have Aether or
  the gate accept a `complete_task` prefix, or have the contract show the
  exact summary a reviewer without `structured` must write.
- **`strict` review on the `direct` pipeline.** The orchestrator asked for
  three reviewers on a pipeline that declares one, and the lead named one.
  The gate refused it as it should, and the orchestrator recovered by
  switching to `adaptive`. That cost a repair each time. A follow-up could
  have the orchestrator pick `adaptive` when it asks for `strict`, or tell
  the `direct` lead how many reviewers the level needs.
- **The increase is not one cause.** Repairs explain about three quarters
  of the gap ($0.0226 of $0.0307 per run). `strict` review explains part of the rest.
  The tasks every run reviewed at `standard` still cost 21% more.
- **Logs.** The Aether logs hold no errors. They show the usual start-up
  warnings (yolo approvals, no systemd scope, the engine's network limits,
  the agent temp directory's long path) and, in the Haiku runs, Claude
  closing receipts that omitted streamed usage. Haiku run 1 also logged
  once that a runtime was gone before its reaped processes were recorded.

## Evidence

This item's artifact (`release-1.4.11-regate-evidence`) keeps:

- the results and logs of all five runs, with each run's Aether data
  directory;
- the sandbox and analysis scripts (`gate.sh`, `run.sh`, `build.sh`,
  `summarize.py`, `pertask.py`, `pertask_est.py`, `repairs.py`,
  `repaircost.py`, `rolecost.py`, `rolecost_est.py`, `reviewers.py`,
  `reviewshape.py`);
- the build log and the Aether 1.2.10 `SHA256SUMS`;
- the `make ci` and `make e2e` logs;
- the sha256 of every binary.

`make ci` and `make e2e` ran on the commit that adds this record; their
results are in the pull request.

| Binary | sha256 prefix |
|---|---|
| head `archon` (`157fe387`, release profile) | `f99ce6f2` |
| head `archon-bench` (`157fe387`, release profile) | `8217ab26` |
| released Aether 1.2.10 | `47f74df8` |
| released v1.4.9 `archon` (baseline) | `d5bc52af` |
| released v1.4.10 `archon` (baseline) | `110e0c26` |
