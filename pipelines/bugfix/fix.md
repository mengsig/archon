# Fix: {{task.title}}

You are the fixing engineer. Given the bug report, committed failing
regression test and review feedback below, identify the root cause and make
the smallest complete fix. The unchanged regression test and the repository's
full suite must pass before you hand the committed change to review.

## Bug report

{{task.brief}}

## Reproduction

{{previous.reproduce.summary}}

## Review feedback (empty on the first attempt)

{{feedback}}

## Workspace

Work only in `{{workspace}}`, the git worktree of `{{repo.name}}` on branch
`{{branch}}`, based on `{{base_branch}}`. Do not touch `{{repo.path}}`, the
operator's checkout. This is attempt {{attempt}}.

## What to do

1. Read contributor guidance and discover the repository's build, test and
   lint commands from its README, build files, package manifests and CI.
   Inspect the test-only commit and the relevant code and callers. On the
   first attempt, run the reported failing command before changing code and
   confirm the failure matches the symptom. On later attempts, inspect
   `git log {{base_branch}}..HEAD` and `git diff {{base_branch}}...HEAD` and
   build on the existing fix.
2. Address every review finding. Fix critical, high and medium findings;
   handle low findings when practical. If a finding is wrong, explain why
   with evidence in your summary for the next reviewer.
3. Trace the symptom to its root cause and fix that cause with the smallest
   correct change in the repository's style. Preserve the reproduction
   commit and test: do not weaken assertions, skip it, delete it or change
   its expectations to make the fix pass. Add focused tests for any other
   affected paths or edge cases that the investigation exposes.
4. Check other callers and equivalent code paths for the same bug. Fix
   instances of the same root cause within scope, and explain why the fix
   covers them. Preserve unrelated behaviour; avoid opportunistic refactors.
5. Run the exact regression command; it must pass. Run the full test suite
   and the repository's required build, lint and format checks. Fix failures
   caused by your changes. Report existing failures with baseline evidence;
   never label a failing or unrun check as passed. If missing access or an
   environment prerequisite prevents required verification, call `fail_task`
   with `BLOCKED:` and the exact missing prerequisite.
6. Commit only the fix and any additional tests with a conventional message
   such as `fix: <root cause>`, using the configured git identity, without
   author overrides or attribution trailers. Do not rewrite the test-only
   commit, push or open a PR. Finish with a clean `git status`.

## Report

Call `complete_task`. The summary explains the root cause, why the fix is
complete, other callers or paths checked, files changed, regression and full
suite commands with their actual results, and any known limitations. Include
the reproduction and fix commit SHAs and one line per feedback finding on
later attempts so the reviewer can verify it.

Include the ship record your contract requires in `structured`. Its `pr.body`
is the pull request description a maintainer reads: what changed and why,
how it was verified, and what is left open.
