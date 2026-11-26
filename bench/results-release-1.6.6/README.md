# Release 1.6.6 smoke gate

**Smoke verdict: PASS, no decrease.** On the release head (`573dcf28`),
Archon passed **4/4 on published v1.6.5 and 4/4 on the candidate** for
both Haiku and Luna. The only miss in the four full runs was one
direct-Aether control on the old Luna run (`review-seeded-bug`, score 0.5).
That control is the same Aether 1.4.3 on both arms. Two more runs per arm
of that task passed all 8 trials. This is a small smoke, not a statistical
pass.

**Time and cost show no regression.** Haiku's Archon time was 1,261.7 s
(old) against 721.2 s (new), and its cost $0.2896 against $0.2974. Luna's
was 801.0 s against 836.1 s (+4%), with incomplete cost estimates of
$0.0821 against $0.0841. These are single runs. Haiku's gap comes from
one old `quality-ledger-transfer` trial (1,011 s): its build stage ran
13 minutes and then a second, record-repair attempt. It is not a
candidate gain.

## Release scope

Archon 1.6.6 still requires **Aether 1.4.3**; the pin is unchanged. Since
v1.6.5:

- [#501](https://github.com/mengsig/archon/pull/501) doctor, status, update
  and the installer report an Aether older than the required release.
- [#502](https://github.com/mengsig/archon/pull/502) live design acceptance
  compares approved pictures taken the same way (store schema 32).
- [#503](https://github.com/mengsig/archon/pull/503) live design sessions
  are enabled by default.
- [#504](https://github.com/mengsig/archon/pull/504) design preview
  scrolling works everywhere, with no in-flight errors and no stuck input.
- [#505](https://github.com/mengsig/archon/pull/505) the design preview
  hands file pickers, uploads, downloads, dialogs, pickers and new tabs to
  the operator's device.

None of these touches the four smoke tasks' code paths directly: they
run no live design. The gate checks that the release does not regress the
ordinary pipelines.

## Runs

One run at a time, alternating old and new, each in its own private
sandbox. A full run has four Archon tasks and four direct-Aether controls.
Time and cost sum only the Archon trials. `*` marks incomplete
subscription estimates, which are not billed dollars.

| Run | Load5 | Archon | Direct Aether | Archon time | Archon cost |
|---|---:|---:|---:|---:|---:|
| old-haiku-1 | 3.29 | 4/4 | 4/4 | 1,261.7 s | $0.2896 |
| new-haiku-1 | 0.65 | 4/4 | 4/4 | 721.2 s | $0.2974 |
| old-luna-1 | 1.46 | 4/4 | 3/4 | 801.0 s | * $0.0821 |
| new-luna-1 | 0.65 | 4/4 | 4/4 | 836.1 s | * $0.0841 |

| Archon task | Haiku old | Haiku new | Luna old | Luna new |
|---|---:|---:|---:|---:|
| fix-off-by-one | 85.4 s | 85.1 s | 215.3 s | 265.3 s |
| quality-ledger-transfer | 1,011.0 s | 470.9 s | 220.3 s | 310.5 s |
| quality-retry-budget | 125.2 s | 130.2 s | 325.3 s | 225.3 s |
| review-seeded-bug | 40.0 s | 35.1 s | 40.1 s | 35.0 s |

Luna `quality-ledger-transfer` ran on `direct` in the old arm and on
`adaptive` (strict review) in the new arm; the orchestrator chose the
pipeline in each trial. Both passed.

Confirmation, 2 more runs per arm of Luna `review-seeded-bug`:

| Run | Load5 | Archon | Direct Aether |
|---|---:|---:|---:|
| old-luna-rsb-2 | 0.66 | pass (45.1 s) | pass |
| new-luna-rsb-2 | 0.58 | pass (45.1 s) | pass |
| old-luna-rsb-3 | 0.64 | pass (45.1 s) | pass |
| new-luna-rsb-3 | 0.59 | pass (40.1 s) | pass |

## Method

- The candidate is `archon` 1.6.6 built with `cargo build --release --locked
  -p archon -p archon-bench` at `573dcf28`.
- The old arm is the published `archon` v1.6.5 (`x86_64-unknown-linux-musl`
  archive, passing its release's `SHA256SUMS`).
- Both arms use the published Aether v1.4.3 (`x86_64-unknown-linux-musl`,
  passing its release's `SHA256SUMS`).
- Each arm runs its own `archon-bench`: the old arm one built from the
  `v1.6.5` tag, so its archond gets 1.6.5's default configuration (design
  sessions off) rather than 1.6.6's. `bench/` is identical between `v1.6.5`
  and the head.
- Haiku uses `claude-code/claude-haiku-5-5`; Luna uses `codex/gpt-6-luna`
  and `bench/results-smoke/baseline-luna.json`. Medium effort, one trial per
  task, serial, the $7 admission limit. No task, prompt, model, budget,
  grader or timeout was changed.
- The wrapper is 1.6.4's `smoke-run.py`, with this release's paths:

```sh
python3 -u .archon/reports/release-1.6.6/smoke-run.py old haiku old-haiku-1
python3 -u .archon/reports/release-1.6.6/smoke-run.py new haiku new-haiku-1
python3 -u .archon/reports/release-1.6.6/smoke-run.py old luna old-luna-1
python3 -u .archon/reports/release-1.6.6/smoke-run.py new luna new-luna-1
python3 -u .archon/reports/release-1.6.6/smoke-run.py old luna old-luna-rsb-2 review-seeded-bug
# then new-luna-rsb-2, old-luna-rsb-3, new-luna-rsb-3
```

Each run started below load5 20, with HOME, config, data, temporary paths
and sockets of its own. Engine credentials were copied mode 600, never
printed, and removed with the sandbox. Each private aetherd was stopped
through its own socket or recorded PID, and every sandbox root was checked
removed afterwards. The installed daemons and their configuration were not
touched.

| Archive or binary | SHA256 |
|---|---|
| Archon 1.6.5 musl archive | `9285abd3452ab6f062e0fcb3e5000c4f3c776b9f245f0cb9d7561de0c4acfecd` |
| Aether 1.4.3 musl archive | `25cf7df3db41fdab660fc5a5781e82f0f7b4de34990e945dbf62778a8846de5f` |
| Published `archon` 1.6.5 | `34c31d36c0668a78014c382de578d8b0b714745cb3db68e2ad54a6f2b8c787f7` |
| Published `aether` 1.4.3 | `3a795ff2a718e8c227ac66ad6bbefd2cfe1838bfcbf77b37eac10e56ca3cc923` |
| Candidate `archon` 1.6.6 | `b704df2c65621a5983e11769883255d96e88e5f3fc1ceb334622fc666b5d2e02` |
| Candidate `archon-bench` | `5b6119ea8e69c5f757a899acb4cace49bbe63c108dbde20f0b3b6faced858628` |
| `archon-bench` from `v1.6.5` | `52197ab6aed925def2e992bbe88f340845e110d767fdd9c4204f30b5fcdae527` |

## Checks

`CARGO_BUILD_JOBS=3 make ci` passed on `573dcf28` (tree `99b6d3bc`) inside
the host CI slot, started at load5 1.29: 5,688 tests passed, 0 failed, 10
ignored across 130 summaries. Formatting, shell checks, clippy and rustdoc
with warnings denied passed. No test needed a rerun. The commit after it
adds only this file.

The artifact `release-1.6.6-evidence` of item `qi_01M8AT7F6XBDXKXXBR07C7JV6X`
keeps the results, item histories, private daemon logs, checksums, wrapper
and CI logs. Credentials and downloaded binaries are left out.

## make ci on the final head

`CARGO_BUILD_JOBS=2 nice -n 19 ionice -c3 make ci` passed on `3cb6b0ac`
(tree `90d07649`) inside the host CI slot, started at load5 4.42 (the first
attempt, at load5 4.42, was cut off by a harness time limit and rerun from
scratch): 5,724 tests passed, 0 failed, 10 ignored across 130 summaries.
Formatting, shell checks, clippy and rustdoc with warnings denied passed. No
test needed a rerun and no flake was seen. The commit after it adds only this
paragraph.

## make ci after the client accept-check fix

`CARGO_BUILD_JOBS=2 nice -n 19 ionice -c3 make ci` passed on `01091255`
(tree `cb13f2e8`) inside the host CI slot, started at load5 4.83: 5,724 tests
passed, 0 failed, 10 ignored across 130 summaries. Formatting, shell checks,
clippy and rustdoc with warnings denied passed. No test needed a rerun. The
commit after it adds only this paragraph.
