# Release 1.5.1 smoke gate

**Verdict: FAIL (release check: DECREASE), on the Haiku pass rate.** The
release head `8eef65c3` on Aether 1.3.1 passed 3/4 Archon trials. v1.5.0 on
Aether 1.3.0 passed 4/4. The miss is `review-seeded-bug`: the Archon trial
ran to its 900 s time limit and was graded on an empty answer.

**The cause is the bench's Archon arm, not the agents' work.** The head
orchestrator answered the review correctly in 28 s ("**Verdict: do not
ship.** `verify()` accepts forged tokens"). The answer is in its Aether
transcript, where #411 now keeps the operator conversation. The bench never
saw it, because it still reads the orchestrator's replies from archond's
feed:

- `bench/src/arms/archon.rs` counts a trial as settled only when an
  `Orchestrator` feed entry has appeared or an item exists (`observe` and
  `Observation::quiet`). It grades the answer from those feed entries and
  from item summaries (`answer_text`).
- v1.5.0's archond appended each reply to the feed as an `Orchestrator`
  entry (`crates/archon-daemon/src/pump/orchestrator.rs`, via
  `ReplyTracker`). #411 removed that path: on the head, archond writes no
  `Orchestrator` feed entry.
- `review-seeded-bug` is the only smoke task with no queue item, since the
  orchestrator reviews the code itself. So nothing ever settled it, and
  its answer was empty. The other three tasks have items, settle through
  them, and are graded on the repository.

**Isolation.** One run of the head on Aether 1.3.0, the Aether v1.5.0 runs
on, failed the same trial the same way (902 s, 0 operator messages, a
correct answer in the transcript after 29 s). Archon moved, not Aether.

Per the release rule, nothing is fixed here, and the gate stopped after the
isolation run. v1.5.0 run 2 and head run 2 did not run. The miss cannot
pass on this head: the bench's Archon arm waits for a feed entry the head
never writes. The operator decides. One follow-up would be for the bench's
Archon arm to read the conversation (the new `conversation` request)
instead of the feed.

Leaving that trial out, the head is within noise. The other three Archon
trials cost $0.1063 on the head against $0.1174 on v1.5.0 (−9%). Their wall
times were 285 s against 328 s, with no repairs on either arm. The Aether arm
passed 4/4 in every run.

| Model | Arm | Runs | Trials passed | Archon cost/run | Wall/run | Repairs/run | Review cost/run |
|---|---|---|---|---|---|---|---|
| Haiku | v1.5.0, Aether 1.3.0 | 1 | 4/4 | $0.1280 | 363 s | 0 | $0.0248 |
| Haiku | head `8eef65c3`, Aether 1.3.1 | 1 | 3/4 | ≥ $0.1157 (−10%) | 1186 s † | 0 | $0.0193 |
| Haiku | isolation: head `8eef65c3`, Aether 1.3.0 | 1 | 3/4 | $0.1749 (+37%) ‡ | 1389 s † | 0 | $0.0489 |

- Haiku costs are billed. `≥` marks a run in which Aether reported a
  trial's cost as incomplete (`interrupted`): the head's
  `quality-ledger-transfer`.
- `†` Includes the `review-seeded-bug` trial's 900 s time limit. Without it,
  the head averages 285 s and the isolation run 488 s, against v1.5.0's
  328 s for the same three tasks.
- `‡` The isolation run's orchestrator chose `strict` review for
  `quality-ledger-transfer` (3 reviewers, $0.0924). Every other Archon task
  in this gate ran at `standard`, and one run cannot tell this choice from
  noise.
- Review cost is summed from each run's Aether data over every agent with
  role `reviewer`.
- A repair is an Archon run of an item that did not ship. No trial in
  this gate started more than one run.
- **Luna was not run.** By operator decision, this release is gated on
  Haiku alone. Luna runs after the release and is report-only.

## Runs

In the order they ran, one at a time:

| Run | Aether | Archon | Archon cost | Archon wall | Reviewers (cost) | Repairs (cost) | Aether arm | Smoke |
|---|---|---|---|---|---|---|---|---|
| Haiku v1.5.0, run 1 | 1.3.0 | 4/4 | $0.1280 | 363 s | 3 ($0.0248) | 0 | 4/4 | PASS |
| Haiku head, run 1 | 1.3.1 | 3/4 | ≥ $0.1157 | 1186 s | 3 ($0.0193) | 0 | 4/4 | FAIL (timeout) |
| Haiku isolation (head on Aether 1.3.0) | 1.3.0 | 3/4 | $0.1749 | 1389 s | 6 ($0.0489) | 0 | 4/4 | FAIL (timeout) |

Archon's cost and wall time per task, with the review level the
orchestrator chose (`standard` unless marked):

| Task | v1.5.0 1 | head 1 | isolation |
|---|---|---|---|
| `fix-off-by-one` | $0.0329 · 66 s | $0.0323 · 84 s | $0.0278 · 79 s |
| `quality-ledger-transfer` | $0.0415 · 126 s | ≥ $0.0352 · 101 s | $0.0924 · 273 s strict |
| `quality-retry-budget` | $0.0430 · 136 s | $0.0388 · 100 s | $0.0460 · 136 s |
| `review-seeded-bug` | $0.0107 · 35 s, no item | $0.0094 · 901 s **timeout**, no item | $0.0087 · 902 s **timeout**, no item |

For comparison, the 1.5.0 gate's two Haiku head runs cost $0.1166 and
$0.1165 (380 s and 345 s). This gate's v1.5.0 run, at $0.1280 and 363 s,
is close to them.

## Aether

- **Head arm: Aether 1.3.1.** Built from mengsig/aether `origin/next`,
  commit `58156d13b6576a885a6dbe2edf45b67dc14eb17b` (#420), in a read-only
  temp clone, with `cargo build --release --locked -p aether`. `aether
  version` reports `aether 1.3.1 (protocol 26)`. A `hello` to a private
  aetherd from this build is answered with the `conversation` feature.
  The clone held no change after the build.
- **Old arm and isolation: Aether 1.3.0.** The published
  `x86_64-unknown-linux-musl` archive, downloaded with `gh release download
  v1.3.0`. It passed the release's `SHA256SUMS` and its own `.sha256`, and
  the binary reports `aether 1.3.0 (protocol 26)`. A `hello` to it is
  answered without the `conversation` feature.

## Method

The method is 1.5.0's, with the stored baseline `baseline.json` (Haiku).
`bench/` is unchanged between `v1.5.0` and `8eef65c3`, apart from one line
in `bench/src/arms/tests.rs`.

- One trial per task and arm (`archon`, `aether`), medium effort, run
  serially, under the default $7 cap.
- Haiku only, alternating v1.5.0 and head runs, one at a time, each inside
  the host CI slot (`sh scripts/ci-slot.sh`).
- **Load.** A run starts only when the host's 1-minute load is below 10.
  The guard refused v1.5.0 run 1 once (10.4) and head run 1 twice (15.9,
  10.8). The runs started at loads of 5.5, 9.2 and 7.2.
- **Token guard.** A run starts only when the host's Claude token has more
  than an hour left. No run was refused. The isolation run started with
  63 minutes left.
- **Head arm.** `archon` and `archon-bench` were built from a `git archive`
  of `8eef65c3` with `cargo build --release --locked -p archon -p
  archon-bench`. The commit after it adds only this file.
- **Old arm.** The released `archon` 1.5.0 (`x86_64-unknown-linux-musl`
  archive, downloaded with `gh release download v1.5.0`, passing its
  `SHA256SUMS` and `.sha256`, reporting `archon 1.5.0`), with
  `archon-bench` built from a `git archive` of the `v1.5.0` tag, on the
  released Aether 1.3.0 that 1.5.0 shipped with.
- **Sandbox.**
  - Each run had a new private directory under `~/.local/share/archon-bench/`,
    with its own aetherd, Archon and Aether config and data, sockets,
    `TMPDIR` and `HOME`.
  - Everything started through an `env -i` wrapper.
  - The `HOME` held a copy (mode 600) of the Claude credentials and the
    `archon-bench` git identity.
  - The aetherd was stopped by its socket and PID.
  - After each run the whole sandbox, credentials included, was removed.
  - No operator daemon, configuration or data was used.
- **Logs.** The Aether logs hold no errors, only the usual start-up
  warnings (no systemd scope under `env -i`).

## `make ci`

`make ci` ran on `8eef65c3` (tree `342c38f4`) from this item's worktree,
outside `/tmp`. It exited 0. It covered format, shell checks, the local
release orchestration tests, clippy with `-D warnings`, and rustdoc with
`-D warnings`. Its 110 test result lines (unit, integration and doc tests)
show 4219 passed, 0 failed and 10 ignored. No test needed a rerun.

## Evidence

This item's artifact (`release-1.5.1-gate-evidence`) keeps the results and
logs of every run, each run's Aether data directory, the gate, probe and
analysis scripts, the build log, the `SHA256SUMS` files, and the `make ci`
log.

| Binary | sha256 prefix |
|---|---|
| head `archon` (`8eef65c3`, release profile) | `3d668aa7` |
| head `archon-bench` (`8eef65c3`, release profile) | `ef2bac6f` |
| Aether 1.3.1 (`58156d13`, release profile) | `aff7ccfe` |
| released Aether 1.3.0 (old arm, isolation) | `390557fa` |
| released v1.5.0 `archon` (old arm) | `ec90dc5a` |
| `archon-bench` from the `v1.5.0` tag (old arm) | `e3fa6a87` |
