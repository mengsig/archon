# Release 1.4.8 smoke gate

**Verdict: PASS. No confirmed decrease, no cost increase.** The release head
is `8266ebb2` (1.4.8 on Aether 1.2.7). v1.4.7 runs on Aether 1.2.5.

- **Haiku.** Archon passed 8 of 8 trials on both builds.
- **Luna.** Archon passed 10 of 12 trials on the head and 4 of 4 on v1.4.7.
  - The head's first Luna run passed 4/4, but its cost estimate was 38%
    above v1.4.7's.
  - The confirmation run of that case passed 2/4 (FAIL), at a lower cost.
  - The second confirmation passed 4/4, at the lowest cost of all Luna runs.
  - Neither the cost increase nor the pass-rate drop came back. The two
    misses are a Luna reviewer that did not record its verdict and a model
    edge-case miss (see "The head's Luna misses" below).

`make ci` passed on `8266ebb2`, with the inherited Archon and Aether
variables unset: 3556 tests passed, 0 failed and 9 ignored. `make e2e`
passed 28 of 28 browser tests on the same head. The commit after it adds
only this file.

**Cost: no increase.**

- **Haiku (billed).** The head cost a mean of ≥ $0.1403 per Archon run, and
  v1.4.7 $0.1358. The gap is the `quality-ledger-transfer` task, which chose
  `strict` review in all four runs. It cost $0.0857 and $0.0560 on the head,
  and $0.0535 and $0.0806 on v1.4.7. Without that task the means are
  $0.0695 (head) and $0.0687 (v1.4.7).
- **Luna (estimated).** Over its three runs the head's mean estimate was
  $0.0813, against $0.0832 for v1.4.7's one run. The head's runs cost
  $0.1147, $0.0758 and $0.0534.

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku v1.4.7, run 1 | 4/4 | $0.1290 | 346 s | 4/4 | PASS · $0.17 |
| Haiku head, run 1 | 4/4 | $0.1598 | 425 s | 4/4 | PASS · $0.20 |
| Haiku v1.4.7, run 2 | 4/4 | $0.1426 | 401 s | 4/4 | PASS · $0.18 |
| Haiku head, run 2 | 4/4 | ≥ $0.1208 | 335 s | 4/4 | PASS · ≥ $0.16 |
| Luna v1.4.7 | 4/4 | $0.0832 * | 480 s | 4/4 | PASS · $0.09 * |
| Luna head, run 1 | 4/4 | $0.1147 * | 611 s | 3/4 | PASS · $0.13 * |
| Luna head, run 2 (confirmation) | 2/4 | $0.0758 * | 445 s | 4/4 | FAIL · $0.09 * |
| Luna head, run 3 (confirmation) | 4/4 | $0.0534 * | 300 s | 4/4 | PASS · $0.06 * |

- **Cost marks.** Haiku's costs are billed. Luna's (`*`) are Aether's
  estimates for a subscription engine, which bills $0, so they are not
  bounds. `≥` marks a cost that Aether reported as incomplete; the true cost
  is at least that. Here that is head run 2's `fix-off-by-one` trial
  (`interrupted`, at least $0.0252).
- **Gate cost.** These eight runs cost ≥ $0.70 billed and $0.37 estimated.

## The head's Luna misses

Luna head run 2 missed two tasks. Neither fails on code that changed since
v1.4.7.

- **`fix-off-by-one`.** The run delivered nothing.
  - Its three reviewers each passed the change: the first, one after the
    automatic repair retry and one after a later repair.
  - None recorded its verdict in `structured`. Each put the verdict JSON in
    its summary, followed by other text, with no fenced block and no
    `VERDICT:` line.
  - `Verdict::read_result` (`crates/archon-pipeline/src/review.rs`) cannot
    read that, so the stage failed with "its result carries no readable
    verdict". That code is unchanged since v1.4.7, which refuses the same
    result.
  - Earlier gates recorded the same Luna reviewer miss: 1.4.5, and the first
    Luna baseline attempt in `results-smoke/`.
  - Aether 1.2.7's changelog has no change to how a Codex agent completes a
    task.
- **`quality-retry-budget`.** The run shipped a reviewed change that failed
  one of 14 hidden tests: `retry_delay(3, 0, base_ms=3, cap_ms=13)` returned
  13 where 12 is expected (exact saturation at the cap). This is the model's
  miss on one of the quality checks.
- **Confirmation.** Luna head run 3 passed all four tasks. The three that
  queued work ran `direct` with `standard` review; `review-seeded-bug` was
  answered with no item, as in every run.

## Method

The method is the 1.4.7 gate's, with the same scripts (in the evidence,
`scripts/gate.sh` and `scripts/run.sh`):

- one trial per task and arm (`archon`, `aether`);
- medium effort, run serially, under the default $7 cap;
- the stored baselines `baseline.json` (Haiku) and `baseline-luna.json`
  (Luna).

Under `bench/`, the baselines, tasks and graders are byte-identical in
v1.4.7 and on the head. Between them `bench/` changed only by two lines of a
unit test (`bench/src/arms/tests.rs`). The runs alternated between v1.4.7
and the head, one at a time. The two confirmations ran after the round.

- **Head arm.**
  - `archon` and `archon-bench` were built from `8266ebb2` with
    `cargo build --release -p archon -p archon-bench --locked`.
  - Aether is the released Aether 1.2.7 (`x86_64-unknown-linux-musl`
    archive). It passed both its release's `SHA256SUMS` and its own
    `.sha256` check, and reports `aether 1.2.7`.
- **v1.4.7 arm.**
  - The released `archon` 1.4.7 (`x86_64-unknown-linux-musl` archive, from
    `gh release download v1.4.7`). The archive passed both the release's
    `SHA256SUMS` and its own `.sha256` check.
  - `archon-bench` was built from the `v1.4.7` tag.
  - Aether is the released Aether 1.2.5, which 1.4.7 was released with. It
    passed the same two checks and reports `aether 1.2.5`.
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
  than an hour left. No run was refused.

## Findings

- **Aether protocol.** Aether 1.2.7 serves protocol 17 to 24, as 1.2.5
  does. Every trial on the head connected to aetherd 1.2.7 at protocol 24,
  and every v1.4.7 trial to aetherd 1.2.5 at protocol 24.
- **Daemon logs.** The archond logs of both builds show only the usual
  tightening of the sandbox's socket directory and, on runs whose agents'
  usage was revised down, "usage total went down; keeping what was already
  charged". No other warning or error appeared.
- **Aether logs.** Both Aether releases log the same warnings:
  - at start-up, yolo approvals, no systemd scope, the engine's network
    limits and the agent temp directory's long path;
  - on every Haiku run, Claude closing receipts that omitted streamed usage;
  - on one Haiku run each (v1.4.7 run 2, head run 1), a runtime gone before
    its reaped processes were recorded.
- **What reaches the agents.** Between v1.4.7 and the head, the stage
  contract and the built-in prompts are unchanged. The MCP tools changed:
  - `model_plan` accepts a `tier` and a `provider` (#296, #297);
  - a stage's message cap and the orchestrator's queue status show agents
    at their cap (#299);
  - `message_orchestrator` says it allows three more escalations after its
    20 messages (#299);
  - a refresh that needed no agent says so (#304).
- **Aether 1.2.7 features.** The head's archond lists `history_expired` in
  its hello. Aether 1.2.7 also offers `subtree_update`.
  - No smoke task steers an item, so `subtree_update` was not exercised.
  - No trial hit expired history.

## Evidence

This item's artifact (`release-1.4.8-gate-evidence`) keeps:

- the results and logs of all eight runs, with each run's Aether data
  directory;
- the sandbox scripts;
- the `make ci` and `make e2e` logs;
- the `SHA256SUMS` of the three releases;
- the sha256 of every binary.

Binary sha256 prefixes:

| Binary | sha256 prefix |
|---|---|
| head `archon` (`8266ebb2`, release profile) | `8c543d82` |
| head `archon-bench` (`8266ebb2`, release profile) | `8abca57d` |
| released v1.4.7 `archon` | `c9f0f8a6` |
| `archon-bench` v1.4.7 tag | `fd969592` |
| released Aether 1.2.7 | `04888830` |
| released Aether 1.2.5 | `0c40c972` |
