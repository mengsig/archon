# Build: {{task.title}}

{{task.brief}}

Work in `{{workspace}}`, on `{{branch}}` based on `{{base_branch}}`, for
`{{repo.name}}`. Do not touch the operator's checkout at `{{repo.path}}`.
Attempt {{attempt}}: preserve earlier work and address all feedback below.

{{feedback}}

Keep ownership of this change from sizing through fixes and verification. The
Delegation section of your stage contract says how children are created, which
models they may use and what each must be told; its Review on record section
says how many reviewers this item needs, which may be none. This prompt says
when a child pays for itself. Reading is most of what a run costs: read what
the change needs once, and hand a child what you know instead of having it
read again.

1. **Size.** Read the repository guidance, then the code the change touches
   and its callers: search first, open the part you need, reread nothing you
   hold.
2. **Delegate what pays.** A worker is for an independent deliverable with
   explicit interfaces and paths it alone owns, when running it beside your
   own work saves more than briefing it, integrating it and the reading it
   repeats. No number of workers is right in itself; often it is none. Do a
   bounded local read yourself instead of sending a child to research it.
   Use leaf workers unless a bounded subproblem benefits from another level
   allowed by the contract; each works in an isolated workspace. Sequence
   dependent parts after their prerequisites and keep tightly coupled work yourself. Give
   each child the model and effort its own task needs, whatever yours are.
   Keep generated outputs ignored and check disk headroom before costly
   builds; never copy build caches or source trees into `/tmp`. Tell every
   child never to touch the operator's checkout at `{{repo.path}}`.
3. **Integrate.** Wait for every worker, integrate the changes you accept
   into your run workspace and resolve conflicts yourself. While you edit,
   run the checks and tests the edit affects, and add regression coverage
   where useful. A blocking or deadlock test must fail in seconds, never hang.
4. **Commit** the settled change on `{{branch}}` under the repository's own
   commit rules (its contributing guide, commit lint, recent history). Where
   it states none: Conventional Commits, a lowercase imperative subject, the
   why in the body. Always: the configured Git identity, never overridden
   with git config flags, `--author`, `GIT_AUTHOR_*` or `GIT_COMMITTER_*`, or
   injected user context; and no attribution of a model or a tool, so no
   `Co-Authored-By` or "Generated with" line, whatever your defaults add.
   Archon delivers your commits as they are. Do not push.
5. **Verify and review at once.** Delegate the reviewers your contract asks
   for on the committed change with `wait: false`: each a fresh leaf that
   shares your workspace and only reads. Several work alone, on different
   models where the policy allows, and never see each other's findings. While
   they read, run the project's build, lint/format and full test suite once,
   and follow your contract's `wait_for` rule. Run it in the foreground or
   wait on it: never end your turn while a check runs. With no reviewer
   asked for, that run is the whole of this step. Hand a reviewer this and
   nothing more:
   - the acceptance criteria, in the few lines that settle what done means;
   - the exact base and head commits (`{{base_commit}}`
     and `git rev-parse HEAD`; for a change stacked on another item's
     branch, the base your contract's Dependencies section defines) and the
     changed paths;
   - the checks you ran, their results and the commit they ran on;
   - the risks you know of;
   - the repository guidance it must apply and only the parts of a spec that
     bear on the change: no conversation, no project history.

   Choose its effort yourself, never below one your contract names: medium
   for ordinary risk, high where the change's risk is medium or more,
   security or an invariant is at stake, or the brief asks for depth. Require the
   verdict your contract shows, on the head it read, with actionable
   findings (path, issue, impact, remedy).
   Pass on your contract's bounds for reviewers and its blocking class:
   only blocking findings fail; the rest are follow-ups. Prefer a model other than your own where the policy in your
   contract allows one; otherwise use your own and say so in the summary. Do
   not create a reviewer per worker.
6. **Fix.** Fix blocking findings and failed checks yourself while context is
   warm, in new commits, and rerun the checks the fix affects. Run the full
   suite again only for a fix whose broad impact you can name, or where the
   project's policy demands it; never because a tree that passed was
   committed. Leave follow-ups open and list them in the `pr.body`.
   A second look is owed after a failed verdict and for any commit after a
   pass: Archon delivers only commits a passing reviewer read, so batch
   fixes. Only the first whole-change read takes as many reviewers as your
   contract asks; each re-review is ONE new agent that reads only the fix
   (`base` the head the earlier reviews read), given the open findings,
   each beside its fix, and the regression evidence, and verifies each is
   fixed with no regression. No new whole-change read is owed. If the
   operator adds scope while a review runs, let it finish, keep its verdict
   for the commit it read, and have the whole change read again with the
   addition. After three failed rounds Archon stops this stage and escalates
   the open blocking findings, so settle a disputed finding with evidence
   rather than another round.
7. **Finish or fail.** An unresolved blocking finding, a failed check, a
   review your contract asks for that did not run, or delegation you needed
   and could not get fail this stage via `fail_task`; use `BLOCKED:` for
   missing access or prerequisites. Advice left open fails nothing. Never
   complete with failed work. Leave a clean working tree, with every child
   finished or cancelled.

Call `complete_task` with the `structured` data your contract requires: the
ship record and, where reviewers were asked for, the review record naming the
agent id of each reviewer the pass rests on. The `pr.body` is what a
maintainer reads: what changed and why, how it was verified, the review's
scope (the commits each reviewer read), advisory findings left open,
assumptions and limits. The `pr.body` names no model or tool: no
`Co-Authored-By` or "Generated with" line. Keep the summary to what the
orchestrator acts on, in a few lines: what changed, the paths, the
verification commands with their results, what workers did, and anything
left open.
