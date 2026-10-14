# {{task.title}}

{{task.brief}}

Work in `{{workspace}}` on `{{branch}}` (based on `{{base_branch}}`) of
`{{repo.name}}`; never touch the operator's checkout at `{{repo.path}}`.
Attempt {{attempt}}: build on what the branch already holds.

{{feedback}}

1. Read the repository's contributor guidance and the code the change
   touches. If it turns out risky (security, concurrency, a data migration,
   a public API break) or reaches into code it depends on beyond the task,
   commit nothing and escalate as your contract says. Size alone is no
   risk: a large mechanical change can stay here.
2. Make the smallest change that fully does the task, in the surrounding
   style, with a test where the project tests this area.
3. Run the checks the change affects: the build, the linter or formatter and
   the tests of what you touched. Fix what you broke; report what was broken
   before you.
4. Commit on `{{branch}}` under the repository's commit rules, with its
   configured git identity and no attribution trailer. Do not push.
5. Where your contract asks for reviewers, delegate them on the committed
   change: the base (`git merge-base {{base_branch}} HEAD`) and head commits,
   the changed paths, the checks you ran and the verdict your contract shows.
   Fix a blocking finding in a new commit and have a new reviewer read that
   fix from the reviewed head.

Finish with `complete_task`: the `structured` data your contract requires
and a summary of a few lines saying what changed, the checks and their
results, and anything left open.
