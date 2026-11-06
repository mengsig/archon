# Product journeys: {{task.title}}

You run every journey of this repository's manifest against the artifact it
ships, with real engines, as a new user and as a returning one would. Test
only: do not fix product code. Archon fails the run if any journey run
fails, is skipped, or does not run.

## What this run checks

{{task.brief}}

## The stack the setup stage prepared (attempt {{attempt}})

{{previous.setup.summary}}

Read `{{workspace}}/.archon/journeys/env.md` for the details and
`{{workspace}}/.archon/journeys.toml` for the manifest.

## Where you are

- Workspace: `{{workspace}}` (branch `{{branch}}` of `{{repo.name}}`).
- Write results and evidence only under
  `{{workspace}}/.archon/reports/journeys/`. Do not edit tracked files.

## How to run

1. **List the runs.** Each journey runs once per install it lists (`empty`
   and `populated` unless it says otherwise) and per engine it lists. A
   run's id is `<journey>/<install>/<engine>`, for example
   `first-agent/empty/claude-code`. These ids are the manifest's list.
2. **Start** private services with `.archon/journeys/start.sh`, if setup
   wrote one.
3. **Run each one in a fresh home.** `.archon/journeys/run.sh <journey>
   <install> <engine>`, one call per run. Never load its environment into
   your own shell. The script drives the real artifact through its real
   interface and exits 0 only when every step passed. Keep its screenshots
   and traces in `$JOURNEY_EVIDENCE`, and its output as `output.txt` there.
   Exit 3 with `ENV-FAULT:` means the private home could not be set up and
   nothing launched: that run is `blocked` by the environment, never a pass.
4. **Check the assertions.** A run passes only when its script exits 0 and
   every `assert` of its journey holds in the evidence. Never pass a run on
   the script alone when the evidence contradicts an assertion.
5. **Retry flakes honestly.** Retry a failed run once, with a new `run.sh`
   call, which makes a fresh home. If
   it passes, it passes, flagged as flaky in its notes. A run that fails
   twice is a failure.
6. **Stop.** Always run `.archon/journeys/stop.sh` at the end, if it exists,
   even after a failure, and confirm that none of what it recorded runs.
7. **Keep the evidence.** Write `.archon/reports/journeys/results.json`:
   `{"runs": [{"id", "journey", "install", "engine", "model", "status",
   "assertions": [{"text", "held"}], "evidence", "notes"}]}`. Publish
   `.archon/reports/journeys/` with `mcp__archon__publish_artifact`, named
   `journeys`.

## Verdict

The verdict says whether this run produced trustworthy results. Archon
judges the product from your scenario list, as the stage contract says.

- **fail** if the stack is at fault: the artifact did not build or launch
  for a reason outside the product, a private service did not start, a
  display was missing, or a run was `blocked` by the environment. The run
  goes back to setup once.
- **pass** otherwise, even when journeys failed because of product defects:
  Archon fails the run on them, with each one named.

Give each run that did not pass a finding with its id, the step or assertion
that failed, expected against actual, and the evidence path. Severity is
high for a journey a user cannot complete. Mark an environment finding
`"blocking": true` and a product defect `"blocking": false`: the scenario
list, not the finding, fails the run on a product defect.

## Report

Call `complete_task` with the structured verdict and findings the stage
contract describes, and beside them `expected` (every run id of the
manifest) and `scenarios` (every run, with `id`, `title`, `status` and
`evidence`). The `summary` gives a table of runs (id, status), the location
of `results.json`, the product defects, and any environment problems. It
ends with `VERDICT: PASS` or `VERDICT: FAIL`.
