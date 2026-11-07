# SWE family and the never-worse gate

The release smoke passes 100%, so it cannot see a quality change (#368). The
SWE family measures quality on real repository issues. It holds each
release to two bars on each model: no worse than the native CLI and no worse
than the best recent Archon release.

## What runs where

- **The set** is Aether's `bench/release-set/swe-v1` (aether#373). It has two
  splits of about 25 instances each, chosen for Haiku and Luna pass rates
  of about 30–70%.
  - **dev** comes from SWE-bench Verified. Its manifest is committed in the
    Aether repository, with the pinned dataset revision, the licence and
    the calibration.
  - **holdout** comes from a decontaminated rolling source, with tasks made
    after the models' cutoffs. It is sealed (gpg-encrypted, mode 0600) under
    `~/.local/share/bench-holdout/swe-v1/` and is never committed, logged or
    reported. It is redrawn every minor release.
- **Aether's runner** owns the trials. It extracts each instance's
  repository at its base commit from the instance image and sends the issue
  as the prompt. It runs each arm and grades with the official
  `FAIL_TO_PASS` and `PASS_TO_PASS` tests in a per-instance container. The
  agent never sees those tests. Its images carry the label
  `archon-bench-swe`, its cache is capped at 30 GB, and it prunes only that
  label.
- **archon-bench** (`archon-bench swe`) builds no task, image or grader of
  its own and keeps no copy of the set. It:
  - picks the arms and hands them to the runner;
  - prints the estimate and the noise bound;
  - reads the runner's aggregate through one adapter (`src/swe/runner.rs`
    and `src/swe/runner/output.rs`);
  - saves its own reduced aggregate (`src/swe/aggregate.rs`, schema
    `archon-bench-swe-aggregate/1`): totals and paired counts only;
  - renders and enforces the gate.

### The runner interface

archon-bench follows Aether's runner as of aether 73f66333 (branch
`archon/615skbgs-bench-swe-tier-with-a-sealed-holdout-and`):

```sh
python3 evals/release_bench.py run --set swe-v1 --split dev|holdout \
  --old <old aether> --new <new aether> \
  --arms haiku/archon-old,haiku/archon-new,haiku/native \
  --archon-pair old=<old archon>,<old aether> --archon-pair new=<new archon>,<new aether> \
  --repeats 1 --jobs 3 --load-max 12 --budget <usd> --work-root <scratch> \
  [--limit N --run-id <id>]   # dev only
```

- **Dev.** The run is named `archon-bench-<random>`. Its aggregate is
  `<checkout>/target/release-bench/runs/<id>/aggregate.json`, with
  per-instance verdicts. archon-bench checks those verdicts against every
  total and pair.
- **Holdout.** The runner names the run itself. archon-bench reads the one
  new `<checkout>/target/release-bench/holdout/<run>/aggregate.json`, which
  carries no verdicts.
- **Aggregate.** Both splits use schema `aether-release-bench-aggregate/1`.
  A pair's `b` counts trial pairs only the baseline passed, and `c` those
  only the candidate passed, matched on task and repeat.
- **What is skipped.** Aether's own `gates`, `overall` and `stop_reason` are
  skipped unread: their reasons may name a task. Its `settings` and its
  per-arm and per-pair derived figures are skipped too.
- **Exit status.** The runner exits nonzero on its own non-PASS verdict.
  When it has written its aggregate, archon-bench reads the file and gates
  for itself.

## Arms

Per model, three arms run on the same instances:

| Arm | Haiku (`claude-code/claude-haiku-5-5`) | Luna (`codex/gpt-6-luna`) |
|---|---|---|
| `old` | The best recent release, Archon v1.4.9 on its released Aether v1.2.8 | the same |
| `new` | The candidate (the #378 head) on its Aether | the same |
| `native` | `claude-code` | `codex` |

Both Archon arms run the `direct` pipeline through the runner's Archon arm
(`--archon-pair`), each on its own `archon` and `aether` binaries, which the
runner pins by SHA-256 into the run.

### The best recent release

`archon-bench swe best-release` ranks
[`release-records.json`](release-records.json):

- **Measure.** Each released `archon`, on the released Aether it shipped
  with, as the old arm of the next release's smoke gate.
- **Window.** v1.4.9 and later, the releases on the current review policy:
  #329's reviewer effort floor, which the operator accepted on
  2026-10-31. Earlier releases ran reviewers at a lower effort that has
  since been replaced.
- **Rule.** Rank by pass rate pooled over both models, then by summed cost
  per run, then the current release first. Only releases measured on both
  models are ranked. An older release is picked only when it passed more,
  or passed as many for less (v1.4.11 ranks above v1.4.12 below).

| Rank | Archon | Aether | Passed | Cost/run (Haiku + Luna) | Source |
|---:|---|---|---:|---:|---|
| 1 | v1.4.9 | v1.2.8 | 16/16 | $0.2372 | `results-release-1.4.10` (Haiku), `results-release-1.4.11` (Luna) |
| 2 | v1.4.11 | v1.2.10 | 15/16 | $0.1713 | `results-release-1.4.12` |
| 3 | v1.4.12 (current) | v1.2.11 | 15/16 | $0.1745 | `results-release-1.5.0` |

v1.4.10 is not ranked: its Luna gate did not run (Codex quota).

The old arm is **v1.4.9 on Aether 1.2.8**. It is the only release in the
window with no miss: v1.4.11 and v1.4.12 each lost one Luna trial in 8.
One trial is well inside the smoke's noise, but the rule is fixed in
advance, and the stricter reference only raises the bar. Install both
from their released assets, checked against `SHA256SUMS` before use:

```sh
dir=$(mktemp -d -p "$HOME/.local/share/archon-bench")
gh release download v1.4.9 -R mengsig/archon -D "$dir/archon" \
  -p 'archon-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS
gh release download v1.2.8 -R mengsig/aether -D "$dir/aether" \
  -p 'aether-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS
archon-bench swe verify-release --sums "$dir/archon/SHA256SUMS" \
  --asset "$dir/archon/archon-x86_64-unknown-linux-musl.tar.gz"
archon-bench swe verify-release --sums "$dir/aether/SHA256SUMS" \
  --asset "$dir/aether/aether-x86_64-unknown-linux-musl.tar.gz"
tar -xzf "$dir/archon/archon-x86_64-unknown-linux-musl.tar.gz" -C "$dir/archon"
tar -xzf "$dir/aether/aether-x86_64-unknown-linux-musl.tar.gz" -C "$dir/aether"
```

`verify-release` exits nonzero for an asset that is missing from
`SHA256SUMS` or does not match it. Check each extracted binary's
`version` before a run.

## The gate

On each model and on each split (dev and holdout), the gate makes two
comparisons:

1. **Never worse than native.** `new` against `native`.
2. **Never worse than the best release.** `new` against `old`.

Each comparison is an exact one-sided paired test (McNemar's test, as a
sign test on the instances only one arm solved) at α = 0.05, fixed before
any run. A comparison **fails** when the reference wins significantly
more of those discordant instances. It is **incomplete**, which also
fails, when:

- its split has no aggregate;
- the pair is missing;
- either arm lost a trial to an infrastructure failure. Rerun those trials;
- fewer than tasks × repeats trial pairs were graded on both arms, as when
  the budget stopped the run.
- it is a `--limit` smoke. The saved aggregate records the limit, so a
  smoke of the first few dev instances never stands in for the whole split.

Any failing comparison fails the release.

**Noise bound**, as `swe run` prints it before every run: a comparison fails
only when the reference wins at least *k* of *d* discordant instances.

| Discordant *d* | 5 | 6 | 7 | 8 | 9 | 10 | 12 | 15 | 20 | 25 |
|---|---|---|---|---|---|---|---|---|---|---|
| Fails at *k* ≥ | 5 | 6 | 7 | 7 | 8 | 9 | 10 | 12 | 15 | 18 |

With fewer than 5 discordant instances no comparison can fail. A drop of
one or two instances in 25 is noise to this gate, and it will not call
one. The arms table reports pass rate with a 95% Wilson interval, cost per
solved instance and wall time per solved instance beside the tests. Those
figures are reported, not gated.

`archon-bench swe gate` writes `gate.md` and `gate.json`, which hold these
tables. A release record (`bench/results-release-<version>/README.md`)
includes `gate.md` as its SWE section, with the native table and the
best-release table as separate tables.

## Running it

Every run goes through the host CI slot, at low priority, one heavy job at a
time:

```sh
export CARGO_BUILD_JOBS=6
cargo build --release -p archon -p archon-bench --locked
sh scripts/ci-slot.sh nice -n 10 ionice -c3 \
  target/release/archon-bench swe run --split dev \
    --aether-checkout "$AETHER_CHECKOUT" \
    --old-label v1.4.9 --old-archon "$dir/archon/archon" --old-aether "$dir/aether/aether" \
    --new-label "$(git rev-parse --short HEAD)" \
    --new-archon "$PWD/target/release/archon" --new-aether "$NEW_AETHER" \
    --jobs 3 --load-max 12 --repeats 1 --budget-usd 90
```

- **Estimate.** `--dry-run` prints the arms, the estimate (cost, time and
  disk), the budget and the noise bound, then stops. `--budget-usd` is
  required: the runner admits no trial past it, and a run it stops short is
  incomplete at the gate, so set it above the estimate. Without measurements the
  estimate uses stated priors (`src/swe/estimate.rs`). Add
  `--estimate-from <earlier>/dev-aggregate.json` to use a run's measured
  means instead.
- **Output.** `swe run` saves only the validated aggregate under
  `bench/results/swe-<split>-<stamp>/` and prints the gate tables of that
  split.
- **Gate.** `swe gate --dev <dev-aggregate.json> --holdout
  <holdout-aggregate.json> --old-label v1.4.9` decides the release, and
  exits nonzero unless every comparison passes.

### The holdout

Only an independent validation item runs the holdout. Agents that change
harness code never read it or run it. It uses the same command with
`--split holdout` and `--holdout-instances <count the seal printed>`.

For that split archon-bench:

- discards the runner's stdout and stderr unread;
- skips Aether's own gate reasons and stop reason unread;
- deletes the scratch directory the runner worked in;
- refuses any path inside the holdout store or its keys
  (`~/.local/share/bench-holdout`, `~/.config/bench-holdout`, and their
  `$XDG_DATA_HOME` and `$XDG_CONFIG_HOME` forms), whatever XDG is set to;
- refuses an aggregate that carries per-instance verdicts or any field
  outside the schema;
- reports a parse error by position only, never by content;
- keeps only the re-serialized aggregate, which has no instance field.

`tests/swe_cli.rs` checks the silence, the scratch removal, the store
refusal and the instance refusal end to end. It uses a stub runner that
leaks a canary on every channel it has, Aether's gate reasons and stop
reason included. The unit tests in `src/swe/runner/output` and
`src/swe/aggregate` check the schema refusal and the position-only parse
errors.

## Dev baseline

**Not run yet.** This change shipped before Aether's runner was pushed and
before its dev manifest and image cache landed (aether#373); it is tested
against a stub of the runner's interface. The baseline is queued as its own
item, under the CI slot, once both changes are in. Its command:

```sh
sh scripts/ci-slot.sh nice -n 10 ionice -c3 \
  target/release/archon-bench swe run --split dev --models haiku,luna \
    --aether-checkout "$AETHER_CHECKOUT" \
    --old-label v1.4.9 --old-archon "$dir/archon/archon" --old-aether "$dir/aether/aether" \
    --new-label "$(git rev-parse --short HEAD)" \
    --new-archon "$PWD/target/release/archon" --new-aether "$NEW_AETHER" \
    --jobs 3 --load-max 12 --repeats 1 --budget-usd 90
```

- **Arms.** Six: `haiku/old`, `haiku/new`, `haiku/native` (claude-code),
  `luna/old`, `luna/new` and `luna/native` (codex). The old arms run
  v1.4.9 on Aether 1.2.8; the new arms run the #378 head on its Aether.
- **Dev manifest.** `$AETHER_CHECKOUT/bench/release-set/swe-v1/manifest.json`.
- **Jobs.** 3, the most the host takes, with the runner starting no trial
  above a 1-minute load of 12.
- **Expected.** 150 trials (25 instances × 6 arms × 1 repeat). The priors
  estimate about 18.8 h and $69, and 34.5 GB of disk (30 GB image cache
  plus workspaces). `--dry-run` prints the same figures before any trial.
- **Budget.** $90, about 1.3 times the estimate, so that a run near the
  priors is not cut short.

To prove the wiring before that, a smoke run of two dev instances on one
model takes about 40 minutes (6 trials) with the priors:

```sh
target/release/archon-bench swe run --split dev --models haiku --limit 2 \
  --budget-usd 6 --aether-checkout "$AETHER_CHECKOUT" ...the same build flags...
```

The smoke's comparisons always read **incomplete**: it shows the
wiring works, and it is no evidence for the gate.
