# Quality smoke baseline

`baseline.json` is the reviewed Haiku baseline: run `20261015T071250Z`,
`claude-code/claude-haiku-5-5` at **medium** effort, $7 admission limit,
**Archon 4/4**. The files in this directory are that run's evidence, and
`archon-bench smoke --check-results bench/results-smoke` passes on them.

| Task | Archon | Aether |
|---|---|---|
| fix-off-by-one | pass · 65 s · ≥ $0.0230 | pass · 21 s · $0.0065 |
| quality-ledger-transfer | pass · 155 s · ≥ $0.0756 | pass · 47 s · $0.0124 |
| quality-retry-budget | pass · 115 s · ≥ $0.0356 | pass · 35 s · $0.0099 |
| review-seeded-bug | pass · 35 s · $0.0096 | pass · 36 s · $0.0089 |

Total spend ≥ $0.1815 (Archon $0.1438, Aether $0.0377); the run took 11.5 min
end to end, of which trial wall time was 370 s for Archon and 139 s for Aether.
Archon trials report `cost_gap = interrupted` because teardown cancels the
orchestrator, so their costs and the total are floors. Archon routed the three
code tasks to `direct` (ledger at strict review, the others standard) and
answered the review task itself without queueing work.

Every hidden grader ran: 8, 10 and 14 hidden tests for the code tasks, and
both rubric groups for the review (each arm named the empty-signature
`startsWith` check in `verify` and the resulting forgery). `solutions/` holds
each trial's diff, `grades/` the grader output, `answers/` the review answers,
`verify.txt` the fixture check of the same checkout and `provenance.json` the
commits, binary hashes and versions.

One trial per cell is a cheap regression signal, not a reliability estimate:
4/4 says Archon lost no pass on this run, not that each task always passes.

## Other evidence from the same session

- [`calibration-high-20261015/`](calibration-high-20261015/summary.md): the
  same preset at **high** effort, started before benchmark runs moved to
  medium. Both arms 4/4, ≥ $0.3177, 29 min. Archon's ledger trial took 762 s
  (strict review) against 155 s at medium. Calibration only, not a baseline.
- [`luna-no-baseline-20261015/`](luna-no-baseline-20261015/summary.md):
  `--model codex/gpt-6-luna --effort medium`, ≥ $0.1228, 18 min. Bootstrap
  refused a candidate, so there is **no Luna baseline**. Archon 2/4 with one
  infrastructure error, Aether 3/4:
  - fix-off-by-one/archon passed its grader, but archond did not exit within
    the 20 s grace after the shutdown request and was killed. The trial counts
    as an infrastructure error. This did not occur with Haiku.
  - quality-ledger-transfer/archon delivered nothing: three runs were refused
    because the Luna reviewers' results carried no structured verdict
    (`answers/quality-ledger-transfer/archon.txt`). The graded `ledger.py` is
    the untouched stub.
  - review-seeded-bug failed for both arms on the rubric group "ties that bug
    to forgery / authentication bypass", although both answers say an empty
    signature makes an unsigned payload trusted. The rubric looks for wording
    such as "forg", "bypass" or "any payload"; this looks like a lexical false
    negative. The grader is unchanged here.

  These are open follow-ups; a Luna baseline needs a new, separately
  authorized run after them.
- [`incomplete-20261014/`](incomplete-20261014/README.md): the first,
  interrupted Haiku attempt (three of eight cells), its Opus calibration and
  the isolation incident account. Nothing there is a baseline.

The suite's history figure (`history.svg`, `history.csv`) covers the v1–v3
snapshots only; the smoke uses a different model and task mix.

## Reproduce in a private sandbox

Build from the checkout being measured. Choose a new sandbox name each time;
nothing below uses or restarts an operator daemon. Run every sandbox command,
including diagnostics, through `sx`, which starts it with `env -i` and only
the private variables: an `archon` command without `ARCHON_DATA_DIR` can
autostart a daemon against operator data. The benchmark starts and tears down
its own archond for each Archon trial; stop the sandbox aetherd yourself.

```sh
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p archon -p archon-bench --locked
sandbox="${XDG_DATA_HOME:-$HOME/.local/share}/archon-bench/q-$(date -u +%H%M%S)"
mkdir -p "$sandbox/bin" "$sandbox/config/aether" "$sandbox/data/aether" \
  "$sandbox/run" "$sandbox/tmp" "$sandbox/archon/config" "$sandbox/archon/data"
cp target/debug/archon "$sandbox/bin/archon"
cat > "$sandbox/sx" <<EOF
#!/bin/sh
exec env -i HOME="$HOME" USER="$USER" LANG="$LANG" \\
  PATH="$sandbox/bin:<provider CLIs, node>:/usr/local/bin:/usr/bin" \\
  AETHER_CONFIG_DIR="$sandbox/config/aether" AETHER_DATA_DIR="$sandbox/data/aether" \\
  AETHER_SOCKET="$sandbox/run/aetherd.sock" \\
  ARCHON_CONFIG_DIR="$sandbox/archon/config" ARCHON_DATA_DIR="$sandbox/archon/data" \\
  ARCHON_SOCKET="$sandbox/run/archond.sock" TMPDIR="$sandbox/tmp" "\$@"
EOF
chmod +x "$sandbox/sx"
"$sandbox/bin/archon" setup --print > "$sandbox/config/aether/config.toml"
printf '[aether]\nmode = "shared"\nsocket = "%s"\n' "$sandbox/run/aetherd.sock" \
  > "$sandbox/archon/config/config.toml"
setsid "$sandbox/sx" aether daemon --foreground > "$sandbox/run/aetherd.log" 2>&1 &
# Wait until `"$sandbox/sx" aether status --socket "$sandbox/run/aetherd.sock"` is ready.
target/debug/archon-bench verify
"$sandbox/sx" "$PWD/target/debug/archon-bench" smoke --bootstrap \
  --archon-bin "$sandbox/bin/archon" --aether-socket "$sandbox/run/aetherd.sock"
"$sandbox/sx" aether stop --socket "$sandbox/run/aetherd.sock"
```

Replace `<provider CLIs, node>` with the directories holding `aether`,
`claude`/`codex` and `node`; `sx` drops `CLAUDECODE` and every other
inherited variable. For Luna add `--model codex/gpt-6-luna --effort medium`.
The gate run is the same command without `--bootstrap` (with
`--baseline <luna baseline>` for Luna, once one exists).

The default admission limit is $7. The runner admits one trial at a time and
checks spend after completion; an in-flight trial can overshoot. The smoke
check refuses an over-budget total, unpriced trials, incomplete cells,
timeouts and infrastructure failures. Keep a failed run for diagnosis instead
of lowering the baseline.

Bootstrap does not read or overwrite `baseline.json`. A complete, priced run
within its cap and with at least one Archon pass writes
`candidate-baseline.json` in its results directory, marked
`"candidate": true`. Review the results, failures and provenance before
copying it to `baseline.json` with `candidate` changed to `false` in a
reviewed commit, keeping `source_run` and `max_usd`. Recheck committed
evidence without a daemon or model call:

```sh
target/debug/archon-bench smoke --check-results bench/results-smoke
```

## What the adversarial fixtures establish

All 11 graders reject untouched fixtures and accept their references
(`verify.txt`). For each quality task, `naive.patch` passes the public
examples and fails hidden checks; the integration tests enforce this.

- Ledger: overflow must leave the source unchanged, and an unknown destination
  must not debit a valid source. Threaded conservation and deadlock checks are
  additional bounded checks, not a proof of linearizability under every schedule.
- Extraction: a lexical prefix check is insufficient for sibling paths,
  symlinks and overwrites. A bad later entry must leave the earlier entries
  unwritten, so validation has to precede mutation of the whole batch.
- Retry: the deadline is strict, server Retry-After values are not capped by
  local exponential backoff, and huge attempt counts need bounded arithmetic.

The suite measures selected contract violations, not every possible bug.
Extraction stays in the full eleven-task suite; its strict review cost keeps
it out of the smoke preset.
