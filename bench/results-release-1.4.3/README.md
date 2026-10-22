# Release 1.4.3 smoke gate

**Verdict: PASS, no decrease.** On the release head `575308c0` (1.4.3) Haiku
passed 8/8 Archon trials in the two gate runs, the same as v1.4.2 (8/8). Luna
passed 3/4 on its first head run against 4/4 on v1.4.2. The miss was a rubric
wording miss on `review-seeded-bug`, and a confirming Luna run on the head
passed 4/4. A third Haiku head run, made to check a cost difference, passed
4/4 at the lowest Archon cost of any Haiku run. `CARGO_BUILD_JOBS=6 make ci`
passed on the same head (3227 tests passed, 0 failed, 9 ignored).

| Run | Archon | Archon cost | Archon wall | Aether | Smoke |
|---|---|---|---|---|---|
| Haiku head, run 1 | 4/4 | $0.1073 | 323 s | 4/4 | PASS · $0.15 |
| Haiku v1.4.2, run 1 | 4/4 | $0.1142 | 382 s | 4/4 | PASS · $0.15 |
| Haiku head, run 2 | 4/4 | $0.1280 | 371 s | 4/4 | PASS · $0.16 |
| Haiku v1.4.2, run 2 | 4/4 | $0.1037 | 320 s | 4/4 | PASS · $0.14 |
| Luna head, run 1 | 3/4 | $0.0469 * | 266 s | 4/4 | FAIL · $0.06 * |
| Luna v1.4.2 | 4/4 | $0.0718 * | 482 s | 4/4 | PASS · $0.08 * |
| Luna head, run 2 (confirmation) | 4/4 | $0.0590 * | 401 s | 4/4 | PASS · $0.07 * |
| Haiku head, run 3 (confirmation) | 4/4 | $0.0982 | 341 s | 4/4 | PASS · $0.14 |

Haiku's costs are billed. Luna's (`*`) are Aether's estimates for a
subscription engine (billed $0), so they are not bounds. The whole gate cost
$0.74 billed and $0.22 estimated.

## Method

The method is the 1.4.2 gate's: one trial per task and arm (`archon`,
`aether`), medium effort, serial, the default $7 cap, and the stored
baselines `baseline.json` (Haiku) and `baseline-luna.json` (Luna). The
baselines, tasks and graders under `bench/` are byte-identical in v1.4.2 and
on the head. Runs were interleaved (head, v1.4.2, head, ...), one at a time.

- **Head arm:** `archon` and `archon-bench` built (`cargo build -p archon -p
  archon-bench --locked`, dev profile) from `575308c0`, with Aether built
  from mengsig/aether `origin/next` at `039a8a2` (dev profile). That commit
  holds every Aether 1.2.3 change except its release commit `7a5b853`
  (#284), which merged during the gate. Besides versions, the changelog and
  the bench record, `7a5b853` changes only tests, CI scripts and the remote
  web UI's command list, none of which a smoke run reaches. The binary
  therefore reports `aether 1.2.2`. Archon checks Aether's protocol (24), not
  its version, so nothing refused it, and its version was not changed.
- **v1.4.2 arm:** the released `archon` 1.4.2 and Aether 1.2.2
  (`x86_64-unknown-linux-musl` archives, each checked against its release's
  `SHA256SUMS`), under `archon-bench` built from the `v1.4.2` tag.
- **Sandbox:** each run had a new private directory under
  `~/.local/share/archon-bench/`. It held its own aetherd, Archon and Aether
  config and data, sockets, `TMPDIR` and `HOME`, all started through an
  `env -i` wrapper. The `HOME` held copies (mode 600) of the engine
  credentials and the `archon-bench` git identity. The aetherd was stopped by
  its socket and PID, and the whole sandbox, credentials included, was
  removed after each run. No operator daemon, configuration or data was used.

## Findings

- **Pass rate.** Over the gate runs Archon scored 11/12 on the head against
  12/12 on v1.4.2, and 19/20 on the head counting both confirmation runs. The
  single miss is Luna head run 1's `review-seeded-bug`, scored 0.5. Its
  answer named `verify`, the empty-signature prefix match and that the
  payload "is returned as authenticated". The rubric's second check wants
  the bug tied to forgery or authentication bypass in those words, so it
  scored MISS. Every other Luna answer in this gate and in the 1.4.2 gate
  used "forge" and passed. The orchestrator answered that review itself in
  20 s on both builds. The orchestrator prompt has not changed since v1.4.2,
  and the confirming run passed, so this is answer-wording variance, not a
  regression.
- **Cost.** In the two gate runs, the Haiku Archon arm cost a mean $0.1177
  on the head against $0.1090 on v1.4.2. The difference is one trial: in
  head run 2 the orchestrator chose `strict` review for
  `quality-ledger-transfer` ($0.0622 against $0.031-0.040 with `standard`).
  Review levels are the orchestrator's choice, and its prompt has not
  changed. The confirmation run (`standard` everywhere) cost $0.0982, below
  both v1.4.2 runs; over three runs the head's mean is $0.1112. Luna's
  estimates were lower on the head ($0.0469 and $0.0590 against $0.0718).
- **Wall time** matches within run-to-run spread: Haiku 323-371 s on the
  head against 320-382 s; Luna 266 s and 401 s against 482 s.
- **What reaches the agents.** Between v1.4.2 and the head the stage
  contract (`crates/archon-pipeline/src/contract.rs`, the review verdict
  formats of #237) and the swarm prompts (#195) changed. The orchestrator
  prompt did not.

## Evidence

The results, logs, sandbox scripts (`scripts/gate.sh`, `scripts/run.sh`),
the `make ci` log and the sha256 of every binary are kept as this item's
artifact (`release-1.4.3-gate-evidence`). Binary sha256 prefixes: head
`archon` `5dcd89c0`, released v1.4.2 `archon` `a0e5d368`, Aether next
`2640c513`, released Aether 1.2.2 `97616c8e`.
