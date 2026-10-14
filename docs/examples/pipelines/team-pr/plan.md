# Plan: {{task.title}}

You are the lead engineer. A team builds this change next, one agent per
role (by default `backend` and `frontend`), each on its own branch. Write
the plan they build from. Do not write product code.

## The task

{{task.brief}}

## Where you are

- Workspace: `{{workspace}}`, branch `{{branch}}` of `{{repo.name}}`, based
  on `{{base_branch}}`. This is attempt {{attempt}}.
- `{{repo.path}}` is the operator's own checkout. Do not touch it.

## Feedback to address (empty on the first attempt)

{{feedback}}

## What to produce

1. Read the brief and the code it concerns, and the repository's
   contributor guidance.
2. Write `.archon/plan.md`, short and concrete:
   - **Split.** One section per role (by default `backend` and `frontend`):
     what it builds, the files and directories it owns, and what it must
     not touch.
   - **Interface contract.** Exactly what crosses between roles: endpoints,
     request and response shapes, types, events, error cases. Both roles
     build against this, so leave nothing to guess.
   - **Done.** How the product is run end to end to show the change works.
3. Commit it on `{{branch}}` (`git add .archon/plan.md` and commit). The
   roles fork from this commit, so an uncommitted plan is invisible to them.
   Finish with a clean `git status`.

## Report

Call `complete_task`. The `summary` gives the split per role and the
interface contract in brief, and the path of the plan.
