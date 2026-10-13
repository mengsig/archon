# Ship: {{task.title}}

You deliver finished, committed work. Do not change code in this stage.
Your job is to get the commits on `{{branch}}` to their destination, exactly as
the ship mode says, and to report where they landed.

## The task that was implemented

{{task.brief}}

## Inputs

- Workspace: `{{workspace}}` (git worktree of `{{repo.name}}` on branch
  `{{branch}}`, based on `{{base_branch}}`).
- Ship mode: **`{{ship.mode}}`**. Auto-merge allowed: **`{{ship.auto_merge}}`**.
- The lead's implementation, verification and review report, which is the basis of the PR description:

{{previous.build.summary}}

## Review on record (every mode)

The report above ends with a line Archon wrote from the lead's review record,
after finding the reviewer among the agents the lead delegated to:
`REVIEW: pass, by <model> (agent <id>)` or `REVIEW: skipped: <reason>`. Check
it before anything is pushed.

- If the last line is neither, call `fail_task` with `no review on record for
  {{branch}}`. Ship nothing.
- If it says `skipped`, read `git diff {{base_branch}}...HEAD`. Only a change
  that plainly needs no review goes out unreviewed: a typo, a comment,
  documentation wording, a version bump, a rename that changes no behaviour.
  If the diff holds more than that, call `fail_task` with `unreviewed change:`
  and what the diff contains. Ship nothing.

## Preconditions (every mode)

1. You deliver the commits the lead verified and had reviewed, and nothing
   else: commit nothing in this stage. Run `git status`. If an uncommitted
   change clearly belongs to the task, the commits are not the reviewed
   change: call `fail_task` with `uncommitted work on {{branch}}:` and the
   paths. Leave what looks unrelated or accidental (build outputs, scratch
   files) alone and mention it in your summary.
2. Run `git log --oneline {{base_branch}}..HEAD`. If it prints nothing, there
   is nothing to ship: call `fail_task` with `nothing to ship: {{branch}} has
   no commits beyond {{base_branch}}`.
3. Confirm you are on `{{branch}}` (`git branch --show-current`). Never ship
   from any other branch.
4. The report above names the head commit and the tree the lead verified.
   `git rev-parse HEAD^{tree}` must print that tree before anything is
   pushed, also after a reword your contract allows: if it does not, call
   `fail_task` with `not the reviewed tree:` and both hashes. If the report
   names no tree, say so in your summary.

## Mode `pr`

1. Push without force: `git push -u origin {{branch}}`. If the push is
   rejected because the remote branch moved, stop and call `fail_task`. Do
   not force-push and do not rebase published commits.
2. Reuse the PR if one is already open for this branch
   (`gh pr list --head {{branch}} --state open --json url`). Otherwise open
   one:
   `gh pr create --base {{base_branch}} --head {{branch}} --title "<title>" --body-file <file>`.
   Capture and report the exact URL returned by `gh pr create` (or the
   existing PR lookup). Never construct a URL or guess a PR number. A pushed
   branch alone is not a shipped PR; if creation fails, call `fail_task`.
   - Title: a conventional-commit style summary of the change (at most 72
     characters), derived from "{{task.title}}".
   - Body: what changed and why, how it was verified (commands and results
     from the inputs above), each review or test outcome above with its open
     advisory findings, and notable assumptions. End with the line
     `Archon item: {{item.id}}`.
3. If auto-merge is `true`, enable it with `gh pr merge {{branch}} --auto`,
   using the merge method the repository allows (prefer `--squash` when
   several are allowed). Never bypass required checks or reviews. If it is
   `false`, do not merge.

## Mode `local`

Do not push and do not open a PR. Make sure the work is committed on
`{{branch}}` and report the head commit (`git rev-parse HEAD`).

## Missing access

If `origin` is missing, `gh` is not authenticated, or the push is refused for
permissions, call `fail_task` with `BLOCKED:` and name exactly what is missing.

## Report

Call `complete_task` with `structured` data:
`{"pr_url": "<url>", "commit": "<full head sha>"}`. Omit `pr_url` in `local`
mode. The `summary` states the mode, the destination (PR URL or branch and
commit), whether auto-merge was enabled, and anything left uncommitted.
