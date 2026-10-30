# E2E setup: {{task.title}}

You prepare a reproducible end-to-end test environment for this product. The
next stage uses your scripts to start the product, drive it like a real user,
and stop it. It is a separate agent that knows only what you write down, so
make the environment boring, scripted and verified.

## What will be tested

{{task.brief}}

## Where you are

- Workspace: `{{workspace}}`, a git worktree of `{{repo.name}}` on branch
  `{{branch}}` (based on `{{base_branch}}`). This is attempt {{attempt}}.
- Put everything you create under `{{workspace}}/.archon/e2e/`. Do not change
  product code: this pipeline tests the product, it does not fix it.

## Feedback from the failed e2e run (empty on the first attempt)

{{feedback}}

If this section is not empty, the e2e stage could not get trustworthy
results and the run came back to you. For each finding, confirm whether it
was caused by the environment (missing dependency, wrong port, service not
ready, flaky startup, missing seed data) or by the product itself. Fix the
environment causes. Leave product defects alone, list them in your summary
as product defects, and do not paper over them.

## What to produce

1. **Learn how the product runs.** Read the README, the contributor docs, the
   Makefile or package scripts, Dockerfiles or compose files, and the CI
   workflows. Work out the build and start commands, the ports, the required
   environment variables, and any backing services.
2. **`.archon/e2e/start.sh`** installs dependencies if needed, builds, and
   starts the product and its services in the background. It writes PIDs or
   container ids to `.archon/e2e/run/`, sends logs to `.archon/e2e/logs/`, and
   waits with a bounded timeout until a health check passes. It must be
   idempotent: running it twice must not start duplicates.
3. **`.archon/e2e/stop.sh`** stops everything `start.sh` started, even after
   a partial start, by the PIDs, process groups or Compose project `start.sh`
   recorded, and exits 0 when none of them runs. It never stops anything by
   name or pattern (`pkill`, `killall`, `pgrep -f`): that would reach
   processes it did not start, the operator's own daemons included.
4. **`.archon/e2e/env.md`** is for the next agent. It gives the base URL or
   entry command, the test accounts or seed data, the tools available for
   driving the product (browser automation, HTTP client, CLI), where logs go,
   and known quirks.
5. **Seed data** that the brief's scenarios need, created by a script under
   `.archon/e2e/`, never by hand.
6. **Prove it works.** Run `start.sh`, confirm that the product answers (one
   smoke request or command), run `stop.sh`, and confirm that none of the
   processes or containers `start.sh` recorded is left running. Do this twice
   to prove idempotence.

Never use production credentials or production services. If the product
needs a secret or an external account you do not have, call `fail_task` with
`BLOCKED:` and name exactly what the operator must provide.

## Report

Call `complete_task`. The `summary` gives the start and stop commands, the
base URL or entry point, the smoke check you ran and its result, the files you
created, and, on a feedback round, which failures were environment problems
(and fixed) and which were product defects.
