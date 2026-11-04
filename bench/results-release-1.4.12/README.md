# Release 1.4.12 smoke gate

**Verdict: FAIL, on a confirmed Luna cost increase.** The release head
`4e63b2e3` on Aether 1.2.11 cost a mean of $0.0597 per Luna run over three
runs. Two fresh v1.4.11 runs on Aether 1.2.10 cost $0.0493: +21%, beyond the
±10% A/A noise of set v2 on Luna. The third head run was the confirmation:
$0.0666, still above both v1.4.11 runs. Luna's wall time rose from 505 s to
601 s per run (+19%). The pass rate did not fall: 11/12 on the head against
7/8 on v1.4.11, and the one miss on each side is the same grading miss.

Haiku is within noise: $0.1269 per run against $0.1220 (+4%), 8/8 on both.

| Model | Arm | Runs | Trials passed | Archon cost/run | Wall/run | Repairs/run |
|---|---|---|---|---|---|---|
| Haiku | v1.4.11, Aether 1.2.10 | 2 | 8/8 | $0.1220 | 346 s | 0 |
| Haiku | head `4e63b2e3`, Aether 1.2.11 | 2 | 8/8 | $0.1269 (+4%) | 373 s | 0 |
| Luna | v1.4.11, Aether 1.2.10 | 2 | 7/8 | $0.0493 * | 505 s | 0 |
| Luna | head `4e63b2e3`, Aether 1.2.11 | 3 | 11/12 | $0.0597 * (+21%) | 601 s | 0.33 (0, 1, 0) |

`*` Luna costs are Aether's estimates: Codex usage is unbilled. Haiku costs
are billed.

## What drives the Luna increase

- **One genuine review fail.** In head run 2, the `quality-ledger-transfer`
  reviewer returned `request_changes` with a real blocking finding: its
  deadlock test's worker threads were not daemon threads, so a deadlock
  could still hang the test process. The gate refused the lead's pass that
  listed this finding, and the repair shipped. That task cost $0.0372
  against $0.0159 to $0.0288 elsewhere; the refused attempt alone was
  $0.0181. Leaving out the repair, the head mean would be $0.0537 (+9%).
- **Review level.** Head run 3 reviewed `quality-ledger-transfer` at
  `strict`. Every v1.4.11 run reviewed it at `standard`.
- **The stage contract is longer.** #375 adds a "Summary and evidence"
  section to every work stage's contract, and the authoring prompts ask
  the agent to capture a `before` and an `after` and to return `pr.summary`
  and `evidence`. This is the one 1.4.12 change on a stage agent's path,
  and the likeliest source of extra cost. The review gate's code
  (`review.rs`, `executor/review`) is unchanged since 1.4.11. Three head
  runs do not separate the contract's cost from run-to-run variance.
- **Against the 1.4.11 gate's record.** 1.4.11's own passing gate recorded
  $0.0592 per Luna run on its release head; the 1.4.12 head's $0.0597 is
  within 1% of that. Today's two v1.4.11 runs ($0.0534 and $0.0453) are the
  cheapest Luna runs of v1.4.11 on record. The gate compares with them as the
  brief asks, so it fails.

## Pass rate

- Both misses are `review-seeded-bug` graded 0.5: the answer named
  `verify`'s acceptance of a signature prefix but did not tie it to forgery
  or authentication bypass. One was on v1.4.11 (Luna run 2) and one on the
  head (Luna run 2). The task runs no stage (the orchestrator answers it
  directly), so no 1.4.12 change is on its path. The Aether arm, which runs no
  Archon code, missed the same task once (v1.4.11 Luna run 1).

## Runs

In the order they ran, one at a time:

| Run | Aether | Archon | Archon cost | Archon wall | Repairs | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|
| Haiku v1.4.11, run 1 | 1.2.10 | 4/4 | $0.1113 | 310 s | 0 | 4/4 | PASS |
| Haiku head, run 1 | 1.2.11 | 4/4 | $0.1188 | 325 s | 0 | 4/4 | PASS |
| Haiku v1.4.11, run 2 | 1.2.10 | 4/4 | $0.1327 | 382 s | 0 | 4/4 | PASS |
| Haiku head, run 2 | 1.2.11 | 4/4 | $0.1351 | 420 s | 0 | 4/4 | PASS |
| Luna v1.4.11, run 1 | 1.2.10 | 4/4 | $0.0534 * | 490 s | 0 | 3/4 | PASS |
| Luna head, run 1 | 1.2.11 | 4/4 | $0.0401 * | 453 s | 0 | 4/4 | PASS |
| Luna v1.4.11, run 2 | 1.2.10 | 3/4 | $0.0453 * | 520 s | 0 | 4/4 | FAIL (pass rate) |
| Luna head, run 2 | 1.2.11 | 3/4 | $0.0724 * | 705 s | 1 ($0.0181) | 4/4 | FAIL (pass rate) |
| Luna head, run 3 (confirmation) | 1.2.11 | 4/4 | $0.0666 * | 646 s | 0 | 4/4 | PASS |

Archon's cost and wall time per task, with the review level the orchestrator
chose (`standard` unless marked):

| Task | Luna old 1 | Luna old 2 | Luna head 1 | Luna head 2 | Luna head 3 |
|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0167 · 145 s | $0.0124 · 165 s | $0.0103 · 70 s none | $0.0162 · 140 s | $0.0197 · 175 s |
| `quality-ledger-transfer` | $0.0198 · 190 s | $0.0159 · 195 s | $0.0145 · 210 s | $0.0372 · 380 s, 1 repair | $0.0288 · 275 s strict |
| `quality-retry-budget` | $0.0140 · 125 s | $0.0138 · 134 s | $0.0116 · 148 s | $0.0155 · 155 s | $0.0147 · 160 s |
| `review-seeded-bug` | $0.0028 · 30 s | $0.0032 · 25 s **miss** | $0.0037 · 25 s | $0.0035 · 30 s **miss** | $0.0034 · 35 s |

| Task | Haiku old 1 | Haiku old 2 | Haiku head 1 | Haiku head 2 |
|---|---|---|---|---|
| `fix-off-by-one` | $0.0373 · 85 s | $0.0273 · 80 s | $0.0184 · 50 s none | $0.0256 · 70 s |
| `quality-ledger-transfer` | $0.0324 · 95 s | $0.0569 · 137 s strict | $0.0522 · 125 s strict | $0.0547 · 180 s |
| `quality-retry-budget` | $0.0323 · 95 s | $0.0384 · 120 s | $0.0380 · 115 s | $0.0417 · 125 s |
| `review-seeded-bug` | $0.0093 · 35 s | $0.0100 · 45 s | $0.0102 · 35 s | $0.0131 · 45 s |

## Aether 1.2.11

The head arm ran on the published Aether 1.2.11 `x86_64-unknown-linux-musl`
archive, downloaded with `gh release download`. It passed both the release's
`SHA256SUMS` and its own `.sha256` check, and it reports `aether 1.2.11`.
Like 1.2.10 it serves protocols 17 to 25 and keeps store schema 28, so
Archon's `PROTOCOL_VERSION` (24) and `MIN_PROTOCOL_VERSION` (17) stay as
they are.

## Method

The method is 1.4.11's, with the same `gate.sh` and the stored baselines
`baseline.json` (Haiku) and `baseline-luna.json` (Luna, `codex/gpt-6-luna`):

- one trial per task and arm (`archon`, `aether`), medium effort, run
  serially, under the default $7 cap;
- Haiku first, then Luna, alternating v1.4.11 and head runs, one at a time.
- **Load.** The host's 1-minute load was below 10 before each run but the
  first. Haiku v1.4.11 run 1 started at 13.3, just after the release build
  finished. Before Luna head run 3 the load reached 18 from other work on
  the host, and the run waited until it fell to 4.9.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. It refused the first try of Luna head run 1 (58 minutes
  left). The run started an hour later and is the one recorded.
- **Head arm.** `archon` and `archon-bench` were built from a `git archive`
  of `4e63b2e3` with `cargo build --release -p archon -p archon-bench
  --locked`, on Aether 1.2.11. The commit after it changes only this file.
- **Old arm.** The released `archon` 1.4.11 (`x86_64-unknown-linux-musl`
  archive, downloaded with `gh release download v1.4.11`, passing its
  `SHA256SUMS` and `.sha256`), with `archon-bench` built from the `v1.4.11`
  tag, on the released Aether 1.2.10 that 1.4.11 was released with (passing
  its `SHA256SUMS` and `.sha256`).
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
- **Logs.** The Aether logs hold no errors, only the usual start-up
  warnings.

## Evidence

This item's artifact (`release-1.4.12-gate-evidence`) keeps the results and
logs of all nine runs, each run's Aether data directory, the gate scripts,
the build log, the `SHA256SUMS` files, and the `make ci` and `make e2e`
logs.

| Binary | sha256 prefix |
|---|---|
| head `archon` (`4e63b2e3`, release profile) | `7edc8448` |
| head `archon-bench` (`4e63b2e3`, release profile) | `63192185` |
| released Aether 1.2.11 | `f8bc883d` |
| released Aether 1.2.10 (old arm) | `47f74df8` |
| released v1.4.11 `archon` (old arm) | `fd4d7b19` |
| `archon-bench` from the `v1.4.11` tag (old arm) | `474fd990` |
