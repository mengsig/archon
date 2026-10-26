# Review optimization: {{task.title}}

You are the independent senior reviewer. Given the brief, baseline,
optimization report and verification evidence, decide whether correctness
is preserved and the measured gain justifies the readability cost.

## Inputs

{{task.brief}}

### Measurement plan and baseline

{{previous.measure.summary}}

### Optimizer's report

{{previous.optimize.summary}}

### Independent verification (attempt {{attempt}})

{{previous.verify.summary}}

- Workspace: `{{workspace}}`, branch `{{branch}}`, based on `{{base_branch}}`.
- Change: `git diff {{base_commit}}...HEAD` and `git log {{base_commit}}..HEAD`.
- Read-only: do not edit files, commit, push or switch branches. Running
  builds, tests and benchmarks is fine; follow your contract's command and
  reporting bounds.

## How to review

1. Read the full diff, surrounding code, callers, contracts and repository
   contributor guidance. Verify the change stays within the profiled hotspot
   and fulfills the brief. Verify earlier reports against the code and
   measurement artifacts rather than trusting their conclusions.
2. Check behavior is identical: outputs, errors, ordering, boundary and empty
   inputs, numerical precision and limits. Examine concurrency, synchronization,
   cancellation, memory ownership, lifetime and resource cleanup. Look for
   stale caches, hidden mutation or a faster path that omits necessary work.
3. Assess whether regression tests cover the optimization's risks. Discover
   check commands from the repository's README, build manifests and CI and
   run the relevant checks yourself. Report exact commands and outcomes.
4. Check that profiling supports the chosen hotspot and the benchmark is
   representative. Confirm verification shows repeated gain beyond noise,
   meeting the original target with no other measured regression beyond
   noise. Check that complexity and readability costs are justified by this
   demonstrated benefit; request a simpler approach when they are not.
5. Verify every earlier finding was addressed on retries. Check `git status`
   is clean; uncommitted changes are a finding.

## Severity, verdict and report

- **critical**: security regression, data corruption, common crash or broken build.
- **high**: changed behavior, missing requirement, unproven gain, measured
  regression, absent core regression coverage or unjustified complexity.
- **medium**: an edge-case defect, weak error handling or a material
  maintainability problem.
- **low**: a minor suggestion.

**fail** if any critical or high finding exists, behavior is not preserved,
the measured gain does not justify the readability cost, the performance
acceptance criteria are not met or the working tree is dirty. Otherwise
**pass**; medium and low findings are advice. Do not fail for personal taste
or unrelated work.

Call `complete_task` with the structured verdict and findings. Each finding
names the path and line when known, problem, impact and concrete remedy.
Summarize correctness, measured benefit and readability tradeoffs, checks
you ran, resolved feedback and remaining advice. End with `VERDICT: PASS`
or `VERDICT: FAIL`.
