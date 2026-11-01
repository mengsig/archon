# Review: {{task.title}}

{{task.brief}}

Work in `{{workspace}}`, on `{{branch}}`, for `{{repo.name}}`. Do not touch
the operator's checkout at `{{repo.path}}`. Attempt {{attempt}}.

{{feedback}}

You lead the review of an existing pull request. The section on the pull
request under review names it, the head Archon froze and checked out here, and
where the change starts (`{{base_commit}}`). This item changes nothing: commit
nothing, push nothing, edit no file. Archon refuses the review once the branch
moves off that head.

1. **Size.** Read the repository guidance and the pull request's diff from its
   fork point to the head (`git diff {{base_commit}} HEAD`), then the callers
   of what it changes: enough to brief reviewers, not to review it yourself.
2. **Review.** Delegate the reviewers your contract's Review on record
   section asks for, each a fresh leaf that shares this workspace and only
   reads, with `wait: false`. Several work alone, on different models where
   the policy allows, and never see each other's findings. Hand each:
   - what the pull request claims to do, from its title, description and
     linked issue (`gh pr view <url>`), and the acceptance criteria you read
     from them;
   - the exact base `{{base_commit}}` and head (`git rev-parse HEAD`), and the
     changed paths;
   - the repository guidance it must apply, its tests and how to run them;
   - the risks you see, above all for code from outside the project:
     secrets, network or file access, build scripts, CI workflows, new
     dependencies, and anything that runs at install or build time.

   Choose each reviewer's effort, never below one your contract names:
   medium for ordinary risk, high where security, CI or release machinery
   is touched. Require the verdict your
   contract shows, on the head it read, with actionable findings (path,
   issue, impact, remedy), each marked blocking or not by your contract's
   blocking class.
3. **Verify.** While they read, run the project's build, lint and tests on
   this head once, read-only: build output goes to the cache your contract
   names, never into the tree. A failing check is a blocking finding.
4. **Finish.** With every reviewer passed and the checks green, call
   `complete_task` with the review record your contract requires, naming the
   agent id of each reviewer the pass rests on, and no ship record. With a
   blocking finding or a failed check, do not fix anything: call `fail_task`
   with each finding (path, issue, impact, remedy) and the checks' output, so
   the author can address them. Leave a clean tree, every child finished or
   cancelled.

Keep the summary to what the orchestrator acts on: the pull request and head
reviewed, the verdict, the checks with their results, and advisory findings
left open.
