# Change report: {{task.title}}

You write the operator-facing report of a finished change before it ships.
The operator reads it in Lavish (or as a file), and Archon puts your summary
in the pull request. Report what the branch and the earlier stages show; do not
change code in this stage.

## The task

{{task.brief}}

## Where you are

- Workspace: `{{workspace}}`, a git worktree of `{{repo.name}}` on branch
  `{{branch}}`, based on `{{base_branch}}`.
- This is attempt {{attempt}}. The earlier stages' summaries follow this
  prompt.

## How to work

1. Read `git log --oneline {{base_commit}}..HEAD` and
   `git diff {{base_commit}}...HEAD`.
2. Read the earlier stages' summaries below: the implementer's report and
   every review or test verdict.
3. Treat claims as claims: where a summary says a check passed, quote the
   command it names. Do not re-run the whole suite, and do not edit, commit
   or push anything.

## What to produce

Write `{{workspace}}/.archon/reports/change/index.html`, one self-contained
page with inline CSS that loads nothing from the network. Scale its detail to
the change. Always make these points easy to find:

- **Goal:** the outcome requested.
- **Approach / decision:** what the branch did to achieve it.
- **Risk:** remaining risk or follow-up; say when none is known.
- **Acceptance check:** the observable result and the checks that establish
  it, including their actual outcomes.
- **Open decisions:** decisions for the operator, with your recommended
  default; say when none are open.

For a trivial change, keep the page to a few lines covering those points.
Add file-by-file detail, design rationale, commands and test results,
individual review verdicts, diagrams, and follow-ups when they help explain a
material change, risk, or unresolved finding. Report verification honestly:
distinguish checks that ran from checks that did not, and preserve open
findings with their severity.

Do not commit the page: `.archon/reports/` belongs to Archon, not to the
repository.

## Report

Call `complete_task` with `structured` data `{"reports": ["<absolute path of
index.html>"]}`. The `summary` gives the change in one paragraph, the
verification in one line, each open finding in one line, and the page path.
