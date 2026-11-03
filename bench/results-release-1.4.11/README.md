# Release 1.4.11 smoke gate

**Verdict: PASS.** The gate re-ran on `e08af024`, the release with Luna fix 2
(#370) merged from `next`, on Aether 1.2.10. Luna's cost increase is gone,
and Haiku is within noise of v1.4.10.

- **Luna** cost $0.0592 per run over two head runs:
  - 9% below the recorded v1.4.9 baseline ($0.0649);
  - 36% below #370's fresh v1.4.9 runs ($0.0931);
  - 38% below the previous re-gate with #369 ($0.0956).
  - It needed no repairs, against 1.5 and 3.0 per run on v1.4.9, and
    passed 8 of 8 trials.
- **Haiku** cost ≥ $0.1906 per run over three head runs, against $0.1851
  on v1.4.10 (+3%), with 0.33 repairs per run against 1.5.
  - It passed 11 of 12 trials. Run 1 missed `review-seeded-bug`: the Haiku
    orchestrator refused to answer because the prompt names `session-lib`
    and the registered repository is `review-seeded-bug`. That trial ran
    no stage and no Archon code that changed since the release's last
    gate.
  - The first two runs averaged $0.2042 (+10%), so a third run confirmed:
    it passed 4/4 at $0.1635, and the miss did not recur.

The baselines are not re-run. Luna is compared with the first attempt's
v1.4.9 runs on Aether 1.2.8 and with #370's two fresh v1.4.9 runs, also on
Aether 1.2.8. Haiku is compared with the first attempt's v1.4.10 runs on
Aether 1.2.9. Each baseline is the release it was released with.

| Model | Runs | Trials passed | Archon cost/run | Wall/run | Repairs/run | Review cost/run |
|---|---|---|---|---|---|---|
| Luna, head `e08af024` | 2 | 8/8 | $0.0592 | 348 s | 0 (0, 0) | $0.0132 |
| Luna, v1.4.9 (recorded) | 2 | 8/8 | $0.0649 | 378 s | 1.5 (2, 1) | $0.0112 |
| Luna, v1.4.9 (#370, fresh) | 2 | 8/8 | $0.0931 | 495 s | 3.0 (4, 2) | – |
| Haiku, head `e08af024` | 3 | 11/12 | ≥ $0.1906 | 562 s | 0.33 (0, 1, 0) | $0.0732 |
| Haiku, v1.4.10 | 2 | 8/8 | $0.1851 | 540 s | 1.5 (2, 1) | $0.0587 |

## Luna

- **Cost and repairs.** The head runs cost $0.0650 and $0.0534, a mean of
  $0.0592, below both v1.4.9 means ($0.0649 recorded, $0.0931 #370's fresh).
  Run 1 cost more than recorded v1.4.9 run 2 ($0.0582); run 2 cost less than
  both recorded runs. Neither run had a repair or a restate note (#370's note to the lead). Every reviewer result was one the
  gate reads: the unstructured reviewer results in both runs and run 2's
  `"verdict": "approve"` were accepted the first time.
- **Review level.** Run 1 reviewed `quality-ledger-transfer` at `strict`
  ($0.0362) and `quality-retry-budget` at `none` ($0.0115). Run 2 reviewed
  every task at `standard` and cost the least of all Luna runs.
- **Wall time.** 355 and 340 s, against 415 and 340 s (recorded) and a mean
  of 495 s (fresh) on v1.4.9.

## Haiku

- **Pass rate.** 11 of 12 trials. The miss is the run 1
  `review-seeded-bug` answer above, given in 15 s by the orchestrator
  without delegating. Haiku passed this task in the 4 Haiku head runs of
  the earlier attempts, in #370's Haiku run and in both v1.4.10 runs. 1.4.1's gate saw the same kind of
  single Haiku miss (1 in 20) and judged it a flake.
- **Cost.** Every run reviewed `quality-ledger-transfer` at `strict`, as
  both v1.4.10 runs did:
  - Run 2's strict review failed three rounds on real findings: a
    str-subclass `__eq__` that lets a transfer move nothing, aliased keys
    in `snapshot()`, and concurrency tests that pass with the lock removed.
    The stage stopped per contract, and its repair shipped. That task cost
    $0.1926, against $0.0682 to $0.0855 in the other strict runs on both
    releases. The repair is a genuine review fail, not a refused result.
  - On the other three tasks the head cost a mean of $0.0742 per run,
    against $0.1117 on v1.4.10.
- **Wall time.** 350, 761 and 576 s, against 600 and 480 s on v1.4.10.

## Aether arm

Every run passed 4/4. The Aether arm runs no Archon code. It cost $0.037
to $0.044 per Haiku run and about $0.010 (estimated) per Luna run.

## Runs

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Repairs (cost) | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|---|
| Luna head, run 1 | 1.2.10 | 4/4 | $0.0650 * | 355 s | 5 ($0.0148) | 0 | 4/4 | PASS · $0.08 * |
| Haiku head, run 1 | 1.2.10 | 3/4 | ≥ $0.1449 | 350 s | 6 ($0.0469) | 0 | 4/4 | FAIL · ≥ $0.19 |
| Luna head, run 2 | 1.2.10 | 4/4 | $0.0534 * | 340 s | 5 ($0.0116) | 0 | 4/4 | PASS · $0.06 * |
| Haiku head, run 2 | 1.2.10 | 4/4 | ≥ $0.2635 | 761 s | 11 ($0.1148) | 1 ($0.0896) | 4/4 | PASS · ≥ $0.30 |
| Haiku head, run 3 (confirmation) | 1.2.10 | 4/4 | $0.1635 | 576 s | 6 ($0.0578) | 0 | 4/4 | PASS · $0.20 |

- Luna costs (`*`) are Aether's estimates: Codex usage is unbilled. Haiku
  costs are billed. `≥` marks a run in which Aether reported a trial's cost
  as incomplete (an interrupted agent).
- Reviewer costs are summed from each run's Aether data, over every agent
  with role `reviewer`.
- A repair is an Archon run of an item that did not ship. Its cost is
  already in the Archon cost.
- **Gate cost.** The five runs cost $0.69 billed (Haiku) and about $0.14
  estimated (Luna).

Archon's cost and wall time per task, with the review level the
orchestrator chose (`standard` unless marked):

| Task | Luna head 1 | Luna head 2 | Haiku head 1 | Haiku head 2 | Haiku head 3 |
|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0138 · 65 s | $0.0152 · 85 s | $0.0345 · 75 s | $0.0258 · 70 s | $0.0284 · 75 s |
| `quality-ledger-transfer` | $0.0362 · 205 s strict | $0.0214 · 140 s | $0.0713 · 165 s strict | $0.1926 · 565 s strict | $0.0855 · 305 s strict |
| `quality-retry-budget` | $0.0115 · 65 s none | $0.0125 · 95 s | $0.0327 · 95 s | $0.0325 · 90 s | $0.0392 · 155 s |
| `review-seeded-bug` | $0.0036 · 20 s | $0.0043 · 20 s | $0.0064 · 15 s **fail** | $0.0126 · 35 s | $0.0103 · 40 s |

The baselines' per-task numbers are in the earlier attempts' record below.
#370's fresh v1.4.9 runs are in item `qi_01M6EKS7ZH6QBR3VGAQF6V0TST`'s
artifact `restate-note-evidence`: $0.0964 and $0.0898 per run, 551 and
440 s, 4 and 2 repairs, all from reviewer results the gate could not read.

## Earlier attempts (FAIL)

Both earlier gates failed on the same Luna cost increase. Every trial
passed in both.

- **First attempt, `1b8176da` (before #369).** Luna cost $0.0839, $0.0816
  and $0.1226 (confirmation) per run, a mean of $0.0960 against $0.0649 on
  v1.4.9 (+48%), with 3, 2 and 5 repairs and a wall time of 579 s. Haiku
  cost ≥ $0.1942 and $0.1028, a mean of $0.1485 (-20% against v1.4.10).
  The 10 Luna repairs came from reviewer results the gate refused, mostly a
  `VERDICT:` line without `head:` or a JSON verdict with prose after it.
  #369 was written from these.
- **Re-gate, `157fe387` (with #369).** Luna cost $0.1044, $0.0923 and
  $0.0901 (confirmation), a mean of $0.0956 (+47%), with 3, 2 and 2 repairs
  and a wall time of 552 s. Haiku cost $0.1194 and $0.1419, a mean of
  $0.1307 (-29%), with no repairs. Three of the 7 Luna repairs were
  reviewer results in shapes #369 did not read (YAML `verdict: approve`,
  `approve_with_nonblocking_finding`, `complete_task {…}`), and two were
  repair leads naming 1 of 3 strict reviewers. #370 was written from these.

The baselines both attempts and this gate use:

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

The first attempt's record, runs and logs are in item
`qi_01M6DHT8GH8XE3NV0ZQRRSFSJK`'s artifact `release-1.4.11-gate-evidence`,
and the re-gate's in item `qi_01M6E3HJ1ZPDVET92YDWWYAJNH`'s artifact
`release-1.4.11-regate-evidence`.

## Aether 1.2.10

The head arm ran on the published Aether 1.2.10 `x86_64-unknown-linux-musl`
archive, downloaded again for this gate with `gh release download`. It
passed both the release's `SHA256SUMS` and its own `.sha256` check, its
`SHA256SUMS` matches the earlier attempts', and it reports `aether 1.2.10`.
It serves protocols 17 to 25, as 1.2.9 does, and keeps store schema 28.
All 20 head trials connected at protocol 24, so Archon's
`PROTOCOL_VERSION` (24) and `MIN_PROTOCOL_VERSION` (17) stay as they are.

## Method

The method is the earlier attempts', with the same `gate.sh` and the same
stored baselines `baseline.json` (Haiku) and `baseline-luna.json` (Luna,
`codex/gpt-6-luna`):

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the runs went one at a time, in the table's order, with the host's
  1-minute load below 10 before each.

`bench/` on `e08af024` differs from `157fe387` only by the earlier
attempts' record.

- **Head arm.** `archon` and `archon-bench` were built from a `git archive`
  of `e08af024` with `cargo build --release -p archon -p archon-bench
  --locked`. The commit after it changes only this file.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held copies (mode 600) of the engine credentials (Claude
    and Codex) and the `archon-bench` git identity.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. No run was refused.

## Findings

- **#370 removed the Luna repairs.** No head run needed a repair or a
  restate note. The reviewer result shapes behind most of the earlier
  attempts' 17 Luna repairs in 6 runs did not recur, or were read the
  first time.
- **The Haiku `review-seeded-bug` miss.** The orchestrator took the
  prompt's "`session-lib` repository" literally and asked the operator to
  register it, instead of reviewing the only registered repository. One
  miss in 12 head trials and 0 in 8 on v1.4.10 does not separate the
  releases. A follow-up could make the bench prompt name the repository it
  registers.
- **Logs.** The Aether logs hold no errors, only the usual start-up
  warnings.

## Evidence

This item's artifact (`release-1.4.11-gate3-evidence`) keeps:

- the results and logs of all five runs, with each run's Aether data
  directory;
- the sandbox and analysis scripts (`gate.sh`, `run.sh`, `seq.sh`,
  `build.sh`, `ci.sh`, `means.py`, `summarize.py`, `pertask_est.py`,
  `repairs.py`, `repaircost.py`, `rolecost_est.py`, `reviewers.py`,
  `notes.py`);
- the build log and the Aether 1.2.10 `SHA256SUMS`;
- the `make ci` and `make e2e` logs.

`make ci` and `make e2e` ran on the commit that adds this record; their
results are in the pull request.

| Binary | sha256 prefix |
|---|---|
| head `archon` (`e08af024`, release profile) | `484e8d98` |
| head `archon-bench` (`e08af024`, release profile) | `9db0a689` |
| released Aether 1.2.10 | `47f74df8` |
| released v1.4.9 `archon` (baseline) | `d5bc52af` |
| released v1.4.10 `archon` (baseline) | `110e0c26` |
