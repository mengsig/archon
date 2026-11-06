# Independent validation and outcome criteria

The daemon's gates check code and tests, not whether a person can use what
was built. An independent validation item checks that; you never do it.

**Before a hand-off.** Before you tell the operator a user-facing build (an
app, UI, CLI or installer a person runs) is ready to try or test:

- Queue one validation item that `depends_on` every item that built it: the
  one build item, or all of a split request's (then it is the outcome owner
  below; queue no second one). It stacks on their branches, so it tests the
  heads they delivered, whatever their pipeline or ship mode. Never `onto`:
  that takes no `depends_on` and refuses team runs and stacked builds.
- Pipeline, by the build's head, not the base: `release-journey` only when
  the head's `.archon/journeys.toml` has a journey for every outcome to
  check (`git show refs/mirror/<branch>:.archon/journeys.toml` in the
  mirror, `get_item` names the branch; queued before the build ran, when
  the briefs have the build keep or add those journeys), else `e2e-strict`,
  briefed with every outcome as a scenario.
- Independence: its `model_plan` pins a `model` for every stage of its
  pipeline (`list_pipelines`), none a model the build's stages ran
  (`get_item`), so no stage falls back to a learned or default choice. If
  `model_policy` leaves no other model, tell the operator the build cannot
  be validated independently instead of queueing a self-check.
- Report only that item's outcome. A pass counts for the outcomes its
  journeys or scenarios checked: the build is ready to try, with its pull
  request and the report; name any outcome it did not check. On a failure,
  tell the operator what failed, from its report, and that a fix is queued;
  queue it, then a new validation item that `depends_on` the fix. Never say
  "ready" before a pass.
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
- One item owns the end-to-end outcome: the hand-off's validation item,
  which `depends_on` every build item, briefed with the whole request's
  outcome criteria. Its pass is the request's pass; the hand-off waits for
  it.
- A build stage checks its change against its brief's outcome criteria and
  says which it could not check where it ran. Such an unchecked outcome is
  left to the owning item, never assumed met.

Example backlog for "let new users start an agent from the desktop app":

1. `direct`: the first-run screen. Code: a setup view with tests. Outcome:
   a new user sees a "Start an agent" step on first launch.
2. `adaptive`, `depends_on` 1: start the agent and stream its reply.
   Outcome: after "Start", the user sees a reply within 30 seconds.
3. `e2e-strict`, `depends_on` 1 and 2, every stage pinned to another
   provider's model: owns "on a fresh install, a new user starts an agent
   and gets a reply within 30 seconds" (`release-journey` instead once the
   manifest has that journey).
