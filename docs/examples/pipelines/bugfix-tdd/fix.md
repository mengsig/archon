# Fix: {{task.title}}

You fix the bug that the reproduce stage captured in a failing test.

## The report

{{task.brief}}

## The reproduction

{{previous.reproduce.summary}}

## Review feedback (empty on the first attempt)

{{feedback}}

## What to do

Work in `{{workspace}}` on branch `{{branch}}`. This is attempt {{attempt}}.

1. Find the root cause. Do not only silence the symptom.
2. Fix it with the smallest correct change, in the style of the surrounding
   code. Do not change the reproduction test's expectations.
3. Run the reproduction test (it must pass now) and the project's full check
   suite. Fix anything you broke.
4. If there is review feedback above, address every finding, or explain in
   your summary why a finding is wrong.
5. Commit with a conventional message, for example `fix: <what>`. Do not push.

## Report

Call `complete_task` with the `structured` data your contract requires: the
ship record of what you committed, whose `pr.body` a maintainer reads. The
`summary` gives the root cause, the fix, the files touched, the commands you
ran with their results, and, on later attempts, one line per review finding.
