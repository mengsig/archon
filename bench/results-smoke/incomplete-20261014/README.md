# Incomplete quality smoke evidence

> Archived. The measured baseline and its evidence are in
> [`bench/results-smoke/`](../README.md); paths below refer to this attempt's
> original layout.

**No measured smoke baseline was established.** The fresh Haiku run was
interrupted after three of eight cells, following a diagnostic isolation error.
`baseline.template.json` is an unmeasured template and is rejected as a baseline;
there is deliberately no `baseline.json`. The default gate cannot pass until a
complete, reviewed Haiku run supplies it. `smoke --dry-run` still prints the plan.

Completed Haiku cells: Archon fix-off-by-one passed (90 s, $0.0327), Aether
fix-off-by-one passed (35 s, $0.0088), and Archon ledger passed (326 s, $0.1214).
The remaining cells have **no verdict**, not failures or implied passes.
Original measurements in `run.json` and `results.jsonl` are preserved; home paths
in version diagnostics are redacted as recorded in `provenance.json`. The
interrupted manifest has no finish time. The summaries describe only completed
trials.

All-agent accounting is in `accounting.json`: earlier Opus calibration $11.1931992,
Haiku task agents $0.16886092, accidental Opus agents $0.7771906, total
**$12.13925072**. This exceeds the $12 assignment cap by $0.13925072. The monitor
interrupted at $0.7417 fresh-sandbox spend, but shutdown incurred additional usage.
No further paid run was made. `calibration/` preserves both incomplete Opus runs
separately; neither is used as a Haiku baseline.

The isolation error was an ad hoc `archon queue list` diagnostic between trials:
it set private config/socket variables but omitted `ARCHON_DATA_DIR`. That CLI
can autostart a daemon. With the trial daemon gone, it started against operator
data and launched unrelated Opus agents on private Aether. Those agents made
operator queue/carryover changes before shutdown. The private daemons were
stopped and the incident was reported for recovery; no blind rollback was made.
The benchmark process itself had all private variables. The shared run/smoke
preflight now requires all four private config/data directories to be distinct;
diagnostic commands must retain the entire private environment too. Prefer saved
captures for inspection.

The calibration passed both ledger and retry with careful implementations; it
did not demonstrate an accuracy difference between arms or review rescuing a
hidden failure. Every naive fixture still fails hidden checks deterministically.
The Haiku sample is too incomplete to judge task difficulty or compare arms.
Extraction was removed from the small preset after expensive strict-review
calibration, and remains in the full eleven-task suite. No Haiku verdict exists
for retry, extraction or the seeded review task in this attempt.

## Reproduce in a private sandbox

Build from the checkout being measured. Choose a new sandbox name each time;
the commands below do not use or restart any operator daemon. The benchmark
creates and tears down its own Archon daemon for each Archon trial. Keep the
sandbox Aether daemon alive across the paired trials, then stop it explicitly.

```sh
CARGO_PROFILE_DEV_DEBUG=0 cargo build -p archon -p archon-bench --locked
sandbox="${XDG_DATA_HOME:-$HOME/.local/share}/archon-bench/q-$(date -u +%H%M%S)"
mkdir -p "$sandbox/bin" "$sandbox/config/aether" "$sandbox/data/aether" \
  "$sandbox/run" "$sandbox/tmp" "$sandbox/archon/config" "$sandbox/archon/data"
cp target/debug/archon "$sandbox/bin/archon"
export PATH="$sandbox/bin:$PATH"
export AETHER_CONFIG_DIR="$sandbox/config/aether"
export AETHER_DATA_DIR="$sandbox/data/aether"
export AETHER_SOCKET="$sandbox/run/aetherd.sock"
export ARCHON_CONFIG_DIR="$sandbox/archon/config"
export ARCHON_DATA_DIR="$sandbox/archon/data"
export ARCHON_SOCKET="$sandbox/run/archond.sock"
export TMPDIR="$sandbox/tmp"
"$sandbox/bin/archon" setup --print > "$AETHER_CONFIG_DIR/config.toml"
printf '[aether]\nmode = "shared"\nsocket = "%s"\n' "$AETHER_SOCKET" > "$ARCHON_CONFIG_DIR/config.toml"
aether daemon --foreground > "$sandbox/run/aetherd.log" 2>&1 &
sandbox_pid=$!
# Wait until `aether status --socket "$AETHER_SOCKET"` reports ready.
target/debug/archon-bench verify
# Bootstrap the first baseline using the exact preset, then review its results.
target/debug/archon-bench smoke --bootstrap \
  --archon-bin "$sandbox/bin/archon" --aether-socket "$AETHER_SOCKET"
aether stop --socket "$AETHER_SOCKET"
wait "$sandbox_pid"
```

Use a clean daemon environment: omit variables that point into the checkout,
and nested-session markers such as `CLAUDECODE`. Preserve access to your
provider CLI/authentication and the sandbox variables above. The measured run
used `env -i` semantics with only HOME/USER/LANG/PATH and those sandbox paths;
its `PATH` contained the private binary, the provider CLI, Node, Cargo and
system tools. The task prompts, Haiku model and requested high effort were identical for
both arms; Archon may select lower stage effort through its model plan.

A future baseline requires a separately authorized fresh run; this assignment
has exhausted its budget. The default admission limit is $7. The runner admits one trial at a time and
checks spend after completion; an in-flight trial can overshoot. The smoke
check also refuses an over-budget total, unpriced trials, incomplete cells,
timeouts and infrastructure failures. Keep a failed run for diagnosis instead
of lowering the baseline. One trial per cell cannot prove a task always passes
or establish how much review improves reliability.

Bootstrap does not read or overwrite `baseline.json`. A complete, priced run
within its cap and with at least one Archon pass writes `candidate-baseline.json`
in its results directory, marked `"candidate": true`. Review the results,
failures and provenance before copying that file to `baseline.json` with
`candidate` changed to `false` in a reviewed commit. Keep `source_run` and
`max_usd` from the measured run. The gate rejects candidates and any run or
check cap above the reviewed baseline's cap.

After a complete run establishes `baseline.json`, recheck its evidence without
a daemon or model call (the incomplete evidence here cannot pass):

```sh
target/debug/archon-bench smoke --check-results bench/results-smoke
```

## What the adversarial fixtures establish

All 11 graders reject untouched fixtures and accept their references. For each
new task, `naive.patch` passes the public examples and fails hidden checks;
this property is also enforced by the integration tests.

- Ledger: overflow must leave the source unchanged, and an unknown destination
  must not debit a valid source. Threaded conservation and deadlock checks are
  additional bounded checks, not a proof of linearizability under every schedule.
- Extraction: a lexical prefix check is insufficient for sibling paths,
  symlinks and overwrites. A bad later entry must leave the earlier entries
  unwritten, so validation has to precede mutation of the whole batch.
- Retry: the deadline is strict, server Retry-After values are not capped by
  local exponential backoff, and huge attempt counts need bounded arithmetic.

In the archived Opus ledger trial, the reviewer passed the functional implementation
and identified three nonblocking test weaknesses. The worker then changed
only `test_ledger.py` and obtained another review. This is evidence of review
improving tests; it does not establish that review rescued a hidden-grader
failure. The suite measures selected contract violations, not every possible
bug, and one paired trial cannot establish that a task always passes.
