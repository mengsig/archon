# Build task `{{team.role}}`: {{task.title}}

You are one worker of a swarm. Each task of the plan ({{team.roles}}) is
built at the same time by its own worker, in its own worktree and branch;
Archon merges the branches onto `{{branch}}` in plan order once every task
completes. Your task, the paths you own and the interfaces you share are
under "Your role" below. That is all you need: build exactly that.

## Where you are

- Workspace: `{{workspace}}`, your task's worktree. Attempt {{attempt}}.
- Branches of this attempt: {{team.branches}}.

## Feedback to address (empty on the first attempt)

{{feedback}}

On attempt 2 or later, the integrator sent tasks back. When the feedback
names tasks to redo and yours is not one of them, change nothing and
complete at once, saying so. Otherwise fix what it says about your task.

## How to work

1. Change files only within your owned paths. Need a change elsewhere?
   Do not make it: say so in your summary, naming the file and why.
2. Provide each interface exactly as written, and code against the ones
   you consume as written, even before their task has built them.
3. Peers: Archon may send you a team roster with each worker's agent id.
   Message a peer with `send_message` only to settle a question about an
   interface between you, and keep working while you wait; never wait
   for a reply to finish your task. Without a roster or messaging tools,
   work alone from the plan.
4. Run your task's acceptance commands until they pass, plus the
   repository's checks that cover your paths, each in the foreground or
   waiting on it: never end your turn while a check runs.
5. Never commit build caches (`__pycache__`, `*.pyc`) or `.archon/` files; Archon
   leaves caches out and refuses to ship the plan file.
6. Commit on your branch with the repository's commit conventions. Do not
   push. Finish with a clean `git status`.

## Report

Call `complete_task`. The `summary` says what you built, the files
touched, each acceptance command with its result, and any change you
needed outside your paths. If an acceptance command cannot pass without
another task's work, say which and why.
