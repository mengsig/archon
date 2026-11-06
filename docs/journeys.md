# Product journeys

Tests and reviews check code. They don't check that a person can use the
product. A desktop app can pass every unit test, every reviewed diff and an
end-to-end suite run against a fake model in the browser build, and still
fail on a fresh install, where a new user can't start an agent. The
`release-journey` pipeline is the gate for that. It builds the artifact the
repository ships, in a private stack, and runs the repository's named user
journeys in it with real engines, on an empty install and on a populated
one. A journey that doesn't pass fails the run.

Use it for release preparation, and before shipping a change to user-facing
behaviour: queue it against the change's branch or pull request head, and
ship only once that head passes.

```sh
archon queue add --pipeline release-journey --onto 123 "Journeys for #123"
```

`release-journey` pushes nothing, so it can run `onto` any open pull
request (see [Work onto an existing pull request](guide.md#work-onto-an-existing-pull-request)).
The repository needs a manifest at `.archon/journeys.toml`. Without one, the
setup stage stops with `BLOCKED:`.

## Before a hand-off

The orchestrator coordinates; it never builds, runs apps or drives journeys
itself. Before it tells you a user-facing build is ready to try, it queues
an independent validation item that `depends_on` the items that built it.
The item stacks on their branches, so it tests the heads they delivered,
team runs and stacked builds included; only a build that itself went `onto`
a pull request is validated `onto` that build once it has finished. It runs `release-journey` when the build's own head has a
`.archon/journeys.toml` with a journey for every outcome to check, and
`e2e-strict`, with each outcome as a scenario, otherwise. Every one of its
stages is pinned to a model no build stage runs: it is queued with the
build only when the build's stages are pinned too, and otherwise once the
build has finished and its models are known. When the model policy leaves
none, the orchestrator says the build cannot be validated
independently. You hear only that item's outcome: on a pass, the build is
ready, with any outcome the run did not check named; on a failure, what
failed and that a fix is queued, never "ready". A success of an internal change (a refactor, tests,
CI, docs) is relayed as before, without validation.

The orchestrator's `guide(topic=validation)` holds the workflow
([source](../crates/archon-daemon/prompts/topics/validation.md)). Turn the
rule off with `validate_before_handoff = false` under `[orchestrator]`
([configuration](configuration.md)); the orchestrator started next gets the
change.

### Outcome criteria and the item that owns them

When the orchestrator splits a user-facing request into several items, each
brief keeps its code-level criteria and adds the user outcomes it serves,
written so they can be checked. One more item owns the end-to-end outcome:
the hand-off's validation item, which `depends_on` all the others, so the
outcome cannot fall between them. Each build stage checks its change against its brief's
outcome criteria and says which ones its environment could not check,
instead of claiming them.

An example backlog for "let new users start an agent from the desktop app":

| # | Pipeline | Depends on | Code criteria | Outcome criteria |
| --- | --- | --- | --- | --- |
| 1 | `direct` | | a first-run setup view, with tests | a new user sees a "Start an agent" step on first launch |
| 2 | `adaptive` | 1 | start the agent and stream its reply, with tests | after "Start", the user sees a reply within 30 seconds |
| 3 | `e2e-strict`, every stage on another provider's model | 1, 2 | none | on a fresh install, a new user starts an agent and gets a reply within 30 seconds |

Item 3 owns the outcome; it would be `release-journey` once the manifest has
a journey for it. Items 1 and 2 pin their stages to one provider's models
and item 3 to another's, so all three are queued at once. The orchestrator calls the build ready only once
it passes.

## How a run goes

| Stage | What it does |
| --- | --- |
| `setup` | Reads the manifest and builds each journey's artifact with its `build` command. It writes `.archon/journeys/run.sh`, which runs one journey run in a fresh private home and launches nothing when that home cannot be set up, and `start.sh` and `stop.sh` when the journeys need private services. |
| `journeys` | Runs every journey once per install and per engine, each in a fresh home, and keeps screenshots, traces and output under `.archon/reports/journeys/`. It publishes that directory as the item's `journeys` artifact. Its gate is `all_pass`, with [`scenarios = "strict"`](PIPELINES.md#strict-scenarios). |
| `report` | Writes `.archon/reports/journeys/index.html` for Lavish, after a passing run. |

Each run of a journey has the id `<journey>/<install>/<engine>`, for
example `first-agent/empty/claude-code`. The `journeys` stage's structured
result lists every id the manifest calls for as `expected`, and every run
with its status as `scenarios`, so `get_item` and `get_stage` show each one.
archond fails the run itself if any run is not `pass` (it failed, was
skipped or was blocked), if an expected run is missing, or if the list is
empty. A failed journey fails the run at once, with each failed run named in
the error. Only an environment fault, which the stage reports as a `fail`
verdict, goes back to `setup`, once.

## The private stack

A journey run must never touch the daemons installed on the machine, or
their state, or the user's homes. The setup prompt and the stage contract
both say so, and `run.sh` enforces it in practice:

- Each run gets a fresh temporary home under `.archon/journeys/homes/`.
  `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
  `XDG_CACHE_HOME` and `XDG_RUNTIME_DIR` all point into it, so daemons the
  artifact starts get their sockets, ports and state there.
- Configuration is copied without secrets. Engine credentials reach the
  stack only through the environment variables an engine's `env` lists,
  never as copied credential files. If one of them is unset, the run stops
  with `BLOCKED:` and names it.
- `run.sh` fails closed. If the home, a variable, a credential or the
  `populate` script fails, it prints `ENV-FAULT:`, exits 3 and launches
  nothing, and that run counts as blocked by the environment. Setup proves
  this with a sentinel command before any journey runs. Each run is its own
  process: no agent loads a run's environment into its own shell.
- Stop scripts stop only what their start scripts recorded, by PID, process
  group or Compose project, never by name or pattern.

## `.archon/journeys.toml`

```toml
version = 1

[[journey]]
id = "first-agent"
title = "A new user starts their first agent"
artifact = { kind = "desktop", build = "npm ci && npm run package -- --linux dir", path = "dist/linux-unpacked/my-app" }
engines = [
  { engine = "claude-code", model = "claude-sonnet-5-5", env = ["CLAUDE_CODE_OAUTH_TOKEN"] },
  { engine = "codex", model = "gpt-6-astra", env = ["CODEX_API_KEY"] },
]
script = "e2e/journeys/first-agent.spec.ts"
run = "npx playwright test e2e/journeys/first-agent.spec.ts --reporter=line"
installs = ["empty", "populated"]
populate = "e2e/journeys/populate.sh"
assert = [
  "the Start button is enabled on an empty install",
  "the new agent reaches Running within two minutes",
  "no error toast appears",
]

[[journey]]
id = "learn"
title = "Learn starts on a subscription engine"
artifact = { kind = "desktop", build = "npm ci && npm run package -- --linux dir", path = "dist/linux-unpacked/my-app" }
engines = [{ engine = "claude-code", model = "claude-sonnet-5-5", env = ["CLAUDE_CODE_OAUTH_TOKEN"] }]
script = "e2e/journeys/learn.spec.ts"
run = "npx playwright test e2e/journeys/learn.spec.ts --reporter=line"
installs = ["empty"]
assert = ["Learn shows a ready model and starts a lesson"]
```

| Key | Required | Meaning |
| --- | --- | --- |
| `version` | yes | The manifest format; only `1` exists. |
| `[[journey]]` | at least one | One named user journey. |
| `id` | yes | Unique, `[a-z0-9-]`. The first part of each run's id. |
| `title` | yes | What the user does, in their words. |
| `artifact.kind` | yes | `desktop` (a packaged app), `cli` (a binary) or `web` (a served build). |
| `artifact.build` | yes | The command that builds the shipped artifact from the worktree, as the release does: the packaged app, not a development server. |
| `artifact.path` | `desktop`, `cli` | The executable the build produces, relative to the worktree. |
| `artifact.serve`, `artifact.url` | `web` | The command that serves the built site, and the URL it answers on. |
| `engines` | at least one | The real engines and models the journey runs with: `engine`, `model`, and `env`, the credential variables passed through to the private stack. The journey runs once per engine. |
| `script` | yes | The steps: a Playwright spec, or any script that drives the artifact through its real interface. |
| `run` | yes | The command that runs `script`. It exits 0 only when every step passed. |
| `installs` | no | `["empty", "populated"]` by default: a first launch with no data, and a returning user's. The journey runs once per install. |
| `populate` | with `populated` | A script, run in the fresh home before a `populated` run, that seeds a returning user's data through the artifact's own CLI or API. |
| `assert` | at least one | The success assertions. A run passes only when `run` exits 0 and the evidence shows every assertion holding. |

`run` gets these variables, besides the private `HOME` and `XDG_*` ones:

| Variable | Value |
| --- | --- |
| `JOURNEY_ID` | The run's id, `<journey>/<install>/<engine>`. |
| `JOURNEY_INSTALL` | `empty` or `populated`. |
| `JOURNEY_ENGINE`, `JOURNEY_MODEL` | The engine and model of this run. |
| `JOURNEY_ARTIFACT` | The absolute path of the built artifact, or its URL for `web`. |
| `JOURNEY_EVIDENCE` | An empty directory for this run's screenshots, traces and output. |

## Example: a desktop app driven by Playwright `_electron`

`e2e/journeys/first-agent.spec.ts` launches the packaged Electron app, not
the development build, with the private home in its environment. It drives
the window as a user would and keeps a screenshot after each step and a
trace of the whole run.

```ts
import { test, expect, _electron as electron } from '@playwright/test';

const evidence = process.env.JOURNEY_EVIDENCE!;

test('a new user starts their first agent', async () => {
  const app = await electron.launch({
    executablePath: process.env.JOURNEY_ARTIFACT!,
    // HOME and XDG_* point into the run's private home.
    env: { ...process.env } as Record<string, string>,
  });
  const context = app.context();
  await context.tracing.start({ screenshots: true, snapshots: true });
  try {
    const page = await app.firstWindow();
    await page.screenshot({ path: `${evidence}/1-launch.png` });

    await page.getByRole('button', { name: 'New agent' }).click();
    await page.getByLabel('Engine').selectOption(process.env.JOURNEY_ENGINE!);
    await page.getByLabel('Model').selectOption(process.env.JOURNEY_MODEL!);
    const start = page.getByRole('button', { name: 'Start' });
    await expect(start).toBeEnabled();
    await page.screenshot({ path: `${evidence}/2-ready.png` });

    await start.click();
    await expect(page.getByText('Running')).toBeVisible({ timeout: 120_000 });
    await expect(page.getByRole('alert')).toHaveCount(0);
    await page.screenshot({ path: `${evidence}/3-running.png` });
  } finally {
    await context.tracing.stop({ path: `${evidence}/trace.zip` });
    await app.close();
  }
});
```

On a headless machine, the setup stage starts a private virtual display
(for example `Xvfb` on a free display number, recorded by `start.sh`) and
`run.sh` exports its `DISPLAY`. A journey whose Start button stays disabled
fails `expect(start).toBeEnabled()`. Its run is recorded as `fail`, with
`2-ready.png` and `trace.zip` as evidence, and the `release-journey` run
fails, naming `first-agent/empty/claude-code`.
