# Build: {{task.title}}

{{task.brief}}

Work in `{{workspace}}`, on `{{branch}}` based on `{{base_branch}}`, for
`{{repo.name}}`. Do not touch the operator's checkout at `{{repo.path}}`.
Attempt {{attempt}}: preserve earlier work and address all feedback below.

{{feedback}}

Keep ownership of this change from sizing through fixes and verification. The
Delegation section of your stage contract says how children are created, which
models they may use and what each must be told; this prompt says when to use
them.

1. Read the repository guidance and relevant code. Size the work and its risk.
   For a trivial or mechanical change (a typo, a comment, a version bump, a
   rename that changes no behaviour), implement it, run the project's own
   applicable checks, and skip review with a concrete reason.
2. For separable work, delegate as many parallel workers as useful: each a
   leaf in an isolated workspace, with its own paths and explicit interfaces.
   Sequence dependent parts after their prerequisites; keep tightly coupled
   work yourself. Keep generated outputs ignored and check disk headroom before
   costly builds; never copy build caches or source trees into `/tmp`. Tell
   every child never to touch the operator's checkout at `{{repo.path}}`.
3. Wait for every worker. Integrate the changes you accept into your run
   workspace and resolve conflicts yourself. Verify the whole change with the
   project's own build, lint/format and relevant tests. Add regression
   coverage where useful.
4. Except for the trivial or mechanical case, delegate ONE fresh reviewer over
   the entire integrated diff against `{{base_branch}}`, every worker's
   changes included: a leaf that shares your workspace and only reads. Give it
   the brief, repository guidance and verification evidence, not your
   conversation. Scale review depth to risk; require a pass/fail verdict and
   actionable findings (path, issue, impact, remedy). Correctness, security
   and missing essential coverage block; style advice does not. Prefer a model
   other than your own where the policy in your contract allows one; otherwise
   use your own and say so in the report. Do not create a reviewer per worker.
5. Fix blocking findings yourself while context is warm, and rerun the affected
   checks. A reviewer that finished cannot look again: delegate a new one for
   the changed areas and their effects. Allow at most two fix-and-review
   rounds. Open findings, failed checks, a review that did not run, or
   delegation you needed and could not get fail this stage via `fail_task`;
   use `BLOCKED:` for missing access or prerequisites. Never complete with
   failed or unreviewed nontrivial work.
6. Commit the verified result on `{{branch}}`, following repository conventions
   and the configured Git identity. Never override identity with git config
   flags, `--author`, `GIT_AUTHOR_*` or `GIT_COMMITTER_*`, or injected user
   context. Do not push; the ship stage handles delivery. Leave a clean working
   tree, with every child finished or cancelled.

Call `complete_task` with the `structured` review record your contract
requires: `"verdict": "pass"` with the model of the reviewer that passed the
change as committed, or `"verdict": "skipped"` with the reason from step 1.
The summary says what changed and why, the paths touched, the exact
verification commands and results, what workers did, the review's scope and
its resolved or advisory findings, and assumptions and remaining limitations.
It becomes the ship stage's PR description input.
