# Independent validation and outcome criteria

The daemon's gates check code and tests, not whether a person can use what
was built. An independent validation item checks that; you never do it.

**Before a hand-off.** Before you tell the operator a user-facing build (an
app, UI, CLI or installer a person runs) is ready to try or test:

- `enqueue` one validation item `onto` the item that built it
  (`onto: {item: "qi_…"}`), so it tests that exact head. Pipeline:
  `release-journey` when the repository's mirror has `.archon/journeys.toml`,
  else `e2e-strict`. Brief it with the user outcomes to check.
- Independence: its `model_plan` names no model the build's stages ran
  (`get_item` shows them); another provider at the same tier counts.
- Report only that item's outcome. On a pass, the build is ready to try,
  with its pull request and the validation report. On a failure, tell the
  operator what failed, from its report, and that a fix is queued; queue it
  (`steer_item` the build, or a new item onto it), then validate the fix's
  head the same way. Never say "ready" before a pass.
- An internal change (refactor, tests, CI, docs, a library no person runs)
  needs none: relay its success as usual.

You never build, run apps or drive journeys yourself or through a helper:
the validation item does.

**Outcome criteria.** When a user-facing request splits into several items:

- Each brief keeps its code-level criteria (functions, tests, files) and
  adds the user outcomes its part serves, written so they can be checked:
  who, on what setup, does what and sees what, within what bound. Example:
  "on a fresh install, a new user starts an agent and gets a reply within
  30 seconds".
- One item owns the end-to-end outcome: a `release-journey` or `e2e-strict`
  validation item that `depends_on` every build item, briefed with the whole
  request's outcome criteria. Its pass is the request's pass; the hand-off
  waits for it.
- A build stage checks its change against its brief's outcome criteria and
  says which it could not check where it ran. Such an unchecked outcome is
  left to the owning item, never assumed met.

Example backlog for "let new users start an agent from the desktop app":

1. `direct`: the first-run screen. Code: a setup view with tests. Outcome:
   a new user sees a "Start an agent" step on first launch.
2. `adaptive`, `depends_on` 1: start the agent and stream its reply.
   Outcome: after "Start", the user sees a reply within 30 seconds.
3. `release-journey`, `depends_on` 1 and 2, on another provider: owns
   "on a fresh install, a new user starts an agent and gets a reply within
   30 seconds".
