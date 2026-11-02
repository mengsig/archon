# Release 1.4.11 smoke gate

**Verdict: FAIL on a confirmed Luna cost increase; Haiku passes. Every trial
passed on both models, but Luna's Archon cost per run rose from a mean of
$0.0649 on v1.4.9 (two runs) to $0.0960 on the head (three runs), +48%. Its
wall time rose from 378 s to 579 s, +53%. The third head run, the
confirmation, was the costliest of all five Luna runs ($0.1226, 701 s). By
the release rule the gate stops here for the operator's decision.** Haiku
cost less on the head: $0.1485 per run against $0.1851 on v1.4.10, -20%,
with wall time 448 s against 540 s. The release head is `1b8176da` (1.4.11)
on Aether 1.2.10. v1.4.10 runs on Aether 1.2.9 and v1.4.9 on Aether 1.2.8,
the releases each was released with.

- **Luna.** Codex returned on 2026-11-02, so 1.4.11 is the first release
  since then and owes the Luna regression check. v1.4.9 is the last release
  that ran Luna; v1.4.10 skipped it.
  - **Pass rate.** The head passed 12 of 12 trials, v1.4.9 8 of 8.
  - **Confirmation.** Head runs 1 and 2 each cost more than both v1.4.9
    runs: $0.0839 and $0.0816, a mean of $0.0828 (+28%). Head run 3
    confirmed it.
- **What drives the Luna increase: refused reviewer results.** Each refusal
  fails the stage, and Archon repairs it with a new lead and a new
  reviewer.
  - Head runs needed 3, 2 and 5 repairs. v1.4.9 runs needed 2 and 1.
  - The runs that did not ship cost $0.0370, $0.0235 and $0.0535 on the
    head, against $0.0209 and $0.0045 on v1.4.9.
  - Without those runs, Archon cost a mean of $0.0580 on the head and
    $0.0522 on v1.4.9 (+11%).
  - Luna reviewers often return no `structured` object on both releases:
    9 of 22 head reviewers and 2 of 8 v1.4.9 reviewers gave one. A
    reviewer without one must write its verdict and a `head:` line in its
    summary.
  - Of the head's 10 refusals:
    - 4 are a `VERDICT:` line with no `head:` line;
    - 2 carry no readable verdict (the verdict JSON is printed in the
      summary with prose after it);
    - 3 are a pass taken back by later text in the summary (see Findings);
    - 1 is a named fail that no passing reviewer answered: the reviewer
      after it gave no readable verdict.
  - v1.4.9's 3 refusals were: no readable verdict, `Verdict: APPROVE`, and
    a ship record without `structured`.
- **A likely cause, not proven.** #351 added a line to the stage contract:
  have each reviewer end its summary with `VERDICT: PASS` or
  `VERDICT: FAIL`. On the head, all 9 run-3 reviewer specs carry it (7 in
  the objective, 2 only in the instructions). No v1.4.9 reviewer spec in
  run 1 (0 of 5) does.
  - On the head, Luna reviewers write that line more often: 14 of 22,
    against 5 of 8 on v1.4.9.
  - They often still leave out `structured` and the `head:` line, which the
    gate refuses.
  - The refusal rate per reviewer is close on both: 10 of 22 on the head,
    3 of 8 on v1.4.9. But each refusal adds a repair with its own reviewer,
    and repairs can fail again: head run 3's `fix-off-by-one` took four
    leads.
- **Review cost.** Reviewers cost a mean of $0.0215 per Luna run on the
  head ($0.0179, $0.0188, $0.0279) against $0.0112 on v1.4.9 ($0.0124,
  $0.0100), and $0.0481 per Haiku run on the head ($0.0763, $0.0198)
  against $0.0587 on v1.4.10 ($0.0657, $0.0517). Every reviewer ran at
  `medium`.
- **Haiku.**
  - **Pass rate.** Both releases passed 8 of 8 trials.
  - **Cost and wall time.** Both were lower on the head, as above. No run
    needed confirming.
  - **Repairs.** The head needed 1 and 0, against 2 and 1 on v1.4.10. All
    v1.4.10 repairs, and the one on the head, are a structured pass that
    the summary was read to take back. #351 targets that refusal, but one
    still occurred (see Findings).
- **Aether arm.** Every run passed 4/4, at $0.035 to $0.038 per Haiku run
  and $0.011 to $0.015 (estimated) per Luna run.

`make ci` passed on `1b8176da`, with the inherited Archon and Aether
variables unset: 3830 tests passed, 0 failed and 9 ignored, in 8 min 34 s.
`make e2e` passed 30 of 30 browser tests on the same head. The commit
after it adds only this file, and `make ci` passed again on the final
tree.

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Repairs (cost) | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|---|
| Haiku v1.4.10, run 1 | 1.2.9 | 4/4 | $0.2068 | 600 s | 8 ($0.0657) | 2 ($0.0553) | 4/4 | PASS · $0.24 |
| Haiku head, run 1 | 1.2.10 | 4/4 | ≥ $0.1942 | 575 s | 9 ($0.0763) | 1 ($0.0757) | 4/4 | PASS · ≥ $0.23 |
| Haiku v1.4.10, run 2 | 1.2.9 | 4/4 | $0.1633 | 480 s | 6 ($0.0517) | 1 ($0.0282) | 4/4 | PASS · $0.20 |
| Haiku head, run 2 | 1.2.10 | 4/4 | $0.1028 | 320 s | 3 ($0.0198) | 0 | 4/4 | PASS · $0.14 |
| Luna v1.4.9, run 1 | 1.2.8 | 4/4 | $0.0715 * | 415 s | 5 ($0.0124) | 2 ($0.0209) | 4/4 | PASS · $0.08 * |
| Luna head, run 1 | 1.2.10 | 4/4 | $0.0839 * | 500 s | 6 ($0.0179) | 3 ($0.0370) | 4/4 | PASS · $0.10 * |
| Luna v1.4.9, run 2 | 1.2.8 | 4/4 | $0.0582 * | 340 s | 3 ($0.0100) | 1 ($0.0045) | 4/4 | PASS · $0.07 * |
| Luna head, run 2 | 1.2.10 | 4/4 | $0.0816 * | 535 s | 7 ($0.0188) | 2 ($0.0235) | 4/4 | PASS · $0.10 * |
| Luna head, run 3 (confirmation) | 1.2.10 | 4/4 | $0.1226 * | 701 s | 9 ($0.0279) | 5 ($0.0535) | 4/4 | PASS · $0.14 * |

- Haiku costs are billed. `≥` marks a cost Aether reported as incomplete:
  in Haiku head run 1, one repair lead's usage ended `interrupted`.
- Luna costs (`*`) are Aether's estimates: Codex usage is unbilled.
- Reviewer costs are summed from each run's Aether data, over every agent
  with role `reviewer`.
- A repair is an Archon run of an item that did not ship. Its cost is
  already in the Archon cost.
- **Gate cost.** The nine runs cost $0.81 billed (Haiku) and about $0.49
  estimated (Luna).

Archon's cost and wall time per task, with the review level the
orchestrator chose (`standard` unless marked):

| Task | Haiku v1.4.10 1 | Haiku v1.4.10 2 | Haiku head 1 | Haiku head 2 |
|---|---|---|---|---|
| `fix-off-by-one` | $0.0539 · 125 s | $0.0271 · 80 s | $0.0279 · 90 s | $0.0274 · 80 s |
| `quality-ledger-transfer` | $0.0785 · 255 s strict | $0.0682 · 190 s strict | $0.1211 · 335 s strict | $0.0317 · 95 s |
| `quality-retry-budget` | $0.0654 · 190 s | $0.0585 · 180 s | $0.0338 · 115 s | $0.0335 · 115 s |
| `review-seeded-bug` | $0.0091 · 30 s | $0.0095 · 30 s | $0.0113 · 35 s | $0.0101 · 30 s |

| Task | Luna v1.4.9 1 | Luna v1.4.9 2 | Luna head 1 | Luna head 2 | Luna head 3 |
|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0224 · 130 s | $0.0177 · 105 s | $0.0256 · 150 s | $0.0420 · 295 s | $0.0593 · 365 s |
| `quality-ledger-transfer` | $0.0282 · 175 s | $0.0198 · 110 s | $0.0292 · 155 s | $0.0198 · 125 s | $0.0407 · 215 s ¹ |
| `quality-retry-budget` | $0.0160 · 85 s | $0.0174 · 110 s | $0.0261 · 175 s | $0.0152 · 95 s | $0.0172 · 95 s |
| `review-seeded-bug` | $0.0050 · 25 s | $0.0033 · 15 s | $0.0031 · 20 s | $0.0046 · 20 s | $0.0055 · 25 s |

¹ Two items: one `standard`, and a follow-up with review `none`.

## Aether 1.2.10

The head arm ran on the published Aether 1.2.10 `x86_64-unknown-linux-musl`
archive, the latest release. It passed both the release's `SHA256SUMS`
and its own `.sha256` check, and it reports `aether 1.2.10`. Its notes list:

- `status` answers within seconds on an overloaded host;
- the build cache stays under its budget;
- a one-shot agent's task stays open while its background jobs run;
- process-sweep, test and `make ci` fixes.

It serves protocols 17 to 25, as 1.2.9 does, and keeps store schema 28.
Every head trial connected at protocol 24, so Archon's `PROTOCOL_VERSION`
(24) and `MIN_PROTOCOL_VERSION` (17) stay as they are.

## Method

The method is the 1.4.10 gate's, with the same scripts (in the evidence,
`scripts/gate.sh` and `scripts/run.sh`), plus the Luna arm the 1.4.9 gate
ran:

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baselines `baseline.json` (Haiku) and `baseline-luna.json`
  (Luna, `codex/gpt-6-luna`).

`bench/` is byte-identical in v1.4.9, v1.4.10 and on the head, apart from
the results record v1.4.10 added. The runs went one at a time, in the
table's order.

- **Head arm.**
  - `archon` and `archon-bench` were built from a `git archive` of
    `1b8176da` with `cargo build --release -p archon -p archon-bench
    --locked`. No build reads the Markdown the later commit adds.
  - Aether is the published 1.2.10, as above.
- **v1.4.10 arm (Haiku).**
  - The released `archon` 1.4.10 (`x86_64-unknown-linux-musl` archive, from
    `gh release download v1.4.10`). The archive passed both the release's
    `SHA256SUMS` and its own `.sha256` check, and reports `archon 1.4.10`.
  - `archon-bench` was built from the `v1.4.10` tag.
  - Aether is the released Aether 1.2.9, which 1.4.10 was released with. It
    passed the same two checks. Its `SHA256SUMS` matches the one the 1.4.10
    gate recorded, its binary hash matches that gate's, and it reports
    `aether 1.2.9`.
- **v1.4.9 arm (Luna).**
  - The released `archon` 1.4.9, from `gh release download v1.4.9`. It
    passed both checks, its `SHA256SUMS` and binary hash match the 1.4.10
    gate's, and it reports `archon 1.4.9`.
  - `archon-bench` was built from the `v1.4.9` tag.
  - Aether is the released Aether 1.2.8, which 1.4.9 was released with. It
    passed both checks. Its `SHA256SUMS` matches the 1.4.9 gate's, its
    binary hash matches that gate's, and it reports `aether 1.2.8`.
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

- **Aether protocol.** Every trial connected at protocol 24: head trials
  to aetherd 1.2.10, v1.4.10 trials to 1.2.9 and v1.4.9 trials to 1.2.8.
- **Passes still refused after #351.** Four head refusals (one Haiku,
  three Luna) were a structured or written pass that later text in the
  summary was read to take back. The words were ordinary explanation:
  - "run after the writes", "cannot deadlock" and "did not lower it" in a
    Haiku reviewer's prose;
  - "with the requested cases confirmed" in a Luna reviewer's;
  - "review found no correctness issues" in another (a `no` before a word
    the reader does not let it deny);
  - a verdict JSON line printed after `VERDICT: PASS` (its `verdict` key
    reads as a second verdict).
  `trails_back` in `crates/archon-core/src/run.rs` holds a pass when any
  later summary line has a word of `HOLDS_PASS`, such as `after`, `only`,
  `not` or `requested`. v1.4.10 and v1.4.9 refuse the same summaries:
  their reader takes any line after a pass as taking it back. So this is no
  regression, but #351 did not close it. A follow-up should read only the
  verdict line and the lines that report a finding, or prefer the
  structured pass when the summary's verdict line agrees with it.
- **Luna reviewers' output.** Luna reviewers often omit `structured`, print
  the verdict JSON in the summary, or write `VERDICT: PASS` with no
  `head:` line. A follow-up could have the stage contract ask for the
  `head:` and `base:` lines beside the `VERDICT:` line it now asks for, or
  have the gate read a verdict JSON line in a summary.
- **Unanswered fail.** In Luna head run 3, one `fix-off-by-one` lead named
  a reviewer that failed its change and a later one whose result had no
  readable verdict. No pass answered the fail, so the gate refused the
  record, as it has since v1.4.9.
- **Review level `none`.** In Luna head run 3, the orchestrator queued a
  follow-up of `quality-ledger-transfer` with review `none`. As in the
  1.4.10 gate, this is the orchestrator's choice.
- **Logs.** The Aether logs show only the usual start-up warnings on all
  three releases: yolo approvals, no systemd scope, the engine's network
  limits and the agent temp directory's long path. They also show Claude
  closing receipts that omitted streamed usage. They hold no errors.

## Evidence

This item's artifact (`release-1.4.11-gate-evidence`) keeps:

- the results and logs of all nine runs, with each run's Aether data
  directory;
- the sandbox scripts, `reviewers.py`, `leadcost.py`, `rolecost.py`, and
  `rolecost_est.py` (estimated cost for Codex agents);
- `repairs.py` and `repaircost.py`, which list the runs that did not ship;
- `reviewshape.py`, which counts reviewer results by shape;
- the `make ci` and `make e2e` logs;
- the `SHA256SUMS` of the five releases;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`1b8176da`, release profile) | `3f9cd47a` |
| head `archon-bench` (`1b8176da`, release profile) | `aeff4ce8` |
| released v1.4.10 `archon` | `110e0c26` |
| `archon-bench` v1.4.10 tag | `b4b90521` |
| released v1.4.9 `archon` | `d5bc52af` |
| `archon-bench` v1.4.9 tag | `713fa12a` |
| released Aether 1.2.10 | `47f74df8` |
| released Aether 1.2.9 | `d97602aa` |
| released Aether 1.2.8 | `72a80507` |
