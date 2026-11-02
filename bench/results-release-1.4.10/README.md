# Release 1.4.10 smoke gate

**Verdict: PASS. Haiku passed 12 of 12 trials on the head and 8 of 8 on
v1.4.9, and Archon's cost per run is flat: a mean of $0.1744 on the head
(three runs) against $0.1723 on v1.4.9 (two runs), +1%. Mean wall time is
653 s against 583 s (+12%), all from one head run; the head's median is
435 s.** The release head is `0a0d3fd9` (1.4.10) on Aether 1.2.9. v1.4.9
runs on Aether 1.2.8, which it was released with.

Luna not run: Codex quota exhausted; operator approved a Haiku-only
release; the Luna regression check runs at the first release after Codex
returns.

- **Pass rate.** Every trial passed on both releases. The Aether arm passed
  4/4 in every run, at $0.035 to $0.041 per run on both.
- **Cost and wall time.** Head run 2 cost $0.2805 and took 1164 s, more
  than any v1.4.9 run. By the release rule it was confirmed with one more
  head run (run 3). That run cost $0.1193 and took 360 s, the least of all
  five, so the increase did not reproduce. Head runs 1 and 2 alone average
  $0.2019 and 800 s (+17% and +37%). Run 3 is cheaper partly because its
  orchestrator queued `quality-ledger-transfer` with review `none` (see
  Findings).
- **Review cost.** Reviewers cost a mean of $0.0548 per run on the head
  ($0.0204, $0.1247, $0.0194) against $0.0592 on v1.4.9 ($0.0888,
  $0.0296). Head runs delegated 3, 10 and 3 reviewers, v1.4.9 runs 9 and 4.
  Every reviewer ran at `medium`, the floor since 1.4.9.
- **What drove the variance.** The orchestrator picks each item's review
  level, and `quality-ledger-transfer` dominates every run's cost:
  - Under `standard` it cost $0.0419 / 135 s on v1.4.9 and $0.0337 / 150 s
    on the head.
  - Under `strict` it cost $0.1104 / 455 s on v1.4.9 (run 1: six reviewers
    in two rounds of three, all passing). On the head (run 2) it cost
    $0.1843 / 863 s: three whole-change reviewers, one blocking fail for
    test quality, a fix, and one reviewer of the fix, as #346 intends, so
    four reviewers where the 1.4.9 gate's strict trials took nine. Then the
    gate refused the record of a passing reviewer and an automatic repair
    delegated three more ($0.0573, 200 s). That refusal comes from verdict
    parsing that has not changed since v1.4.9 (see Findings).

`make ci` passed on `0a0d3fd9`, with the inherited Archon and Aether
variables unset: 3756 tests passed, 0 failed and 9 ignored, in 6 min 44 s.
`make e2e` passed 30 of 30 browser tests on the same head. The two commits
after it change only Markdown: the Aether 1.2.9 pin in the install guide
and the changelog, and this file. `make ci` passed again on the final tree.

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|
| Haiku v1.4.9, run 1 | 1.2.8 | 4/4 | $0.2090 | 726 s | 9 ($0.0888) | 4/4 | PASS · $0.25 |
| Haiku head, run 1 | 1.2.9 | 4/4 | $0.1233 | 435 s | 3 ($0.0204) | 4/4 | PASS · $0.16 |
| Haiku v1.4.9, run 2 | 1.2.8 | 4/4 | $0.1355 | 440 s | 4 ($0.0296) | 4/4 | PASS · $0.17 |
| Haiku head, run 2 | 1.2.9 | 4/4 | $0.2805 | 1164 s | 10 ($0.1247) | 4/4 | PASS · $0.32 |
| Haiku head, run 3 (confirmation) | 1.2.9 | 4/4 | $0.1193 | 360 s | 3 ($0.0194) | 4/4 | PASS · $0.16 |

- All costs are billed. Reviewer costs are summed from each run's Aether
  data, from the provider-reported usage of every agent with role
  `reviewer`.
- **Gate cost.** These five runs cost $1.06.

Archon's cost and wall time per task (Haiku, billed), with the review level
the orchestrator chose:

| Task | v1.4.9 run 1 | v1.4.9 run 2 | head 1 | head 2 | head 3 |
|---|---|---|---|---|---|
| `fix-off-by-one` | $0.0523 · 120 s | $0.0413 · 125 s | $0.0322 · 100 s | $0.0266 · 85 s | $0.0456 · 125 s |
| `quality-ledger-transfer` | $0.1104 · 455 s strict | $0.0419 · 135 s | $0.0337 · 150 s | $0.1843 · 863 s strict | $0.0261 · 85 s none |
| `quality-retry-budget` | $0.0343 · 100 s | $0.0412 · 140 s | $0.0458 · 140 s ¹ | $0.0578 · 170 s | $0.0370 · 110 s |
| `review-seeded-bug` | $0.0120 · 50 s | $0.0112 · 40 s | $0.0115 · 45 s | $0.0117 · 45 s | $0.0107 · 40 s |

Levels are `standard` unless marked. ¹ Two items: one `standard`, and a
follow-up of tests with review `none`.

## Aether 1.2.9

Aether 1.2.9 was published during this gate, so the head arm ran on the
published `x86_64-unknown-linux-musl` archive. It passed both the release's
`SHA256SUMS` and its own `.sha256` check, and it reports `aether 1.2.9`.
It serves protocols 17 to 25, as 1.2.8 does, and every head trial connected
at protocol 24. Its notes list only TUI fixes and a docs page, with no
change to the daemon, the protocol (25) or the store.

## Method

The method is the 1.4.9 gate's, with the same scripts (in the evidence,
`scripts/gate.sh` and `scripts/run.sh`), minus the Luna model and its Codex
credentials:

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baseline `baseline.json` (Haiku).

`bench/` is byte-identical in v1.4.9 and on the head. The runs went one at
a time, in the table's order.

- **Head arm.**
  - `archon` and `archon-bench` were built from a `git archive` of
    `0a0d3fd9` with `cargo build --release -p archon -p archon-bench
    --locked`. No build reads the Markdown the later commits change.
  - Aether is the published 1.2.9, as above.
- **v1.4.9 arm.**
  - The released `archon` 1.4.9 (`x86_64-unknown-linux-musl` archive, from
    `gh release download v1.4.9`). The archive passed both the release's
    `SHA256SUMS` and its own `.sha256` check, and reports `archon 1.4.9`.
  - `archon-bench` was built from the `v1.4.9` tag.
  - Aether is the released Aether 1.2.8, which 1.4.9 was released with. It
    passed the same two checks, its `SHA256SUMS` matches the one the 1.4.9
    gate recorded, its binary hash matches that gate's, and it reports
    `aether 1.2.8`.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held a copy (mode 600) of the Claude credentials and the
    `archon-bench` git identity; no Codex credentials were copied.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. The first run waited about 40 minutes for the host to
  renew it; no run was refused.

## Findings

- **Aether protocol.** Every head trial connected to aetherd 1.2.9 at
  protocol 24, and every v1.4.9 trial to aetherd 1.2.8 at 24.
- **Refused passing reviews (not a 1.4.10 regression).** Four of the 14
  items that had a review needed an automatic repair for this, on both
  releases: one in each v1.4.9 run and two in head run 2. Each time the reviewer's
  `structured` said `pass` with no blocking finding, but its summary opened
  with `Verdict: pass.` and went on in the same line. The summary parser
  reads such a line as an unreadable verdict, which takes the structured
  pass back. The parser (`Verdict::from_summary` in
  `crates/archon-core/src/run.rs`) is unchanged since v1.4.9, and a probe
  of it with the head's two refused summaries reproduced the refusal.
  Each repair costs a new lead and new reviewers: $0.016 to $0.057 per item. Head
  run 3 had a fifth repair, a correct one: that reviewer gave no
  `structured` and no `VERDICT:` line. A follow-up should let a `pass`
  label followed by prose agree with a structured pass.
- **Review level `none` on a concurrency task.** Head run 3's orchestrator
  queued `quality-ledger-transfer`, a thread-safety task, with review
  `none`; the change still graded as a pass. The other four runs of this
  gate chose `strict` or `standard` for it. The orchestrator's guidance on
  review levels is unchanged since v1.4.9, so this is a Haiku
  orchestrator's choice; it is worth watching at the next release.
- **Logs.** The Aether logs show the usual start-up warnings on both
  releases.

## Evidence

This item's artifact (`release-1.4.10-gate-evidence`) keeps:

- the results and logs of all five runs, with each run's Aether data
  directory;
- the sandbox scripts, `reviewers.py`, and `leadcost.py` and `rolecost.py`,
  which sum billed cost per stage lead, per reviewer and per role;
- the `make ci` and `make e2e` logs;
- the `SHA256SUMS` of the three releases;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`0a0d3fd9`, release profile) | `bfbcda7b` |
| head `archon-bench` (`0a0d3fd9`, release profile) | `2329e600` |
| released v1.4.9 `archon` | `d5bc52af` |
| `archon-bench` v1.4.9 tag | `5086826b` |
| released Aether 1.2.9 | `d97602aa` |
| released Aether 1.2.8 | `72a80507` |
