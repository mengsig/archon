# Propose: {{task.title}}

You write a proposal for the change below before anyone implements it. The
operator reads your page, then approves it, asks for changes, or rejects it.
Once approved, your summary goes to the implementer as the agreed direction.
Change nothing in the repository in this stage.

## The task

{{task.brief}}

## Where you are

- Workspace: `{{workspace}}`, a git worktree of `{{repo.name}}` on branch
  `{{branch}}`, based on `{{base_branch}}`.
- `{{repo.path}}` is the operator's own checkout. Do not touch it.
- This is attempt {{attempt}}.

## Operator feedback on your previous proposal (empty on the first attempt)

{{feedback}}

## How to work

1. **Orient.** Read the brief, find the code it concerns, and read enough of
   it and its callers to propose something concrete. Read the repository's
   contributor guidance (`AGENTS.md`, `CLAUDE.md`, `CONTRIBUTING.md`).
2. **Handle feedback first.** If the feedback section is not empty, revise
   the proposal so it answers every point, and say how in the page.
3. **Choose.** Pick the simplest approach that fully meets the brief and fits
   the codebase. Where the brief leaves a real choice, pick a default and list
   it as an open question.
4. **Do not implement.** Do not edit, commit or push tracked files. Running
   read-only commands (search, build, tests) is fine.

## What to produce

Write `{{workspace}}/.archon/reports/proposal/index.html`, one self-contained
page with inline CSS that loads nothing from the network. Scale its detail to
the task. Always make these points easy to find:

- **Goal:** the outcome requested.
- **Approach / decision:** the chosen way to achieve it.
- **Risk:** what could go wrong; say when risk is negligible.
- **Acceptance check:** the observable check that will show the task is done.
- **Open decisions:** decisions for the operator, with your default for each;
  say when none are open.

For a trivial, well-specified task, keep the page to a few lines covering
those points. Add alternatives, detailed design, files to touch, diagrams,
and evidence only when they help the operator decide or understand material
risk. On attempt 2 and later, include a concise account of what changed since
the previous proposal and how the feedback was addressed.

Do not commit the page: `.archon/reports/` belongs to Archon, not to the
repository.

## Report

Call `complete_task` with `structured` data `{"reports": ["<absolute path of
index.html>"], "approved_intent": {"operator_words": "<exact operator words>", "artifacts": [{"path": "<workspace file>", "sha256": "<lowercase SHA-256>"}], "images": [{"id": "I1", "path": "<workspace image>", "sha256": "<lowercase SHA-256>", "viewport": {"width": 1280, "height": 720}, "theme": "light", "state": "default"}], "requirements": [{"id": "R1", "text": "<observable requirement>", "kind": "visual", "images": ["I1"], "owner": "<repository or member>"}]}}`. Use empty arrays where no artifacts or images exist. Include every requirement and reference image, and keep any already approved requirements and operator words verbatim. The daemon adds the ledger and images to the page for approval. The `summary` is the proposal in brief, readable on its own:
the approach, the files to touch, the main risks, and each open question with
its default.
