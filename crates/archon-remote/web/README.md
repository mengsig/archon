# Archon web app

The phone (and desktop) app that the remote listener serves: watch the queue,
answer questions, chat with the orchestrator, queue work and pause Archon.
Plain ES modules and CSS, with no framework, no build step and no runtime
dependencies. It runs under a strict CSP (no inline script or style, no eval).

## Layout

```
index.html, manifest.webmanifest, sw.js, icons/   the installable shell
styles/      tokens.css (AA light/dark palettes), base.css, components.css,
             screens.css, layout.css (responsive workspace), graph.css
src/core/    transport, API, daemon requests, feed, conversation, store, router,
             format, theme, live-query (visible-screen reads and teardown)
src/ui/      DOM, components, sheets, toasts, keyed FLIP lists, reduced-motion
             helpers, safe Markdown, graph/ (DAG layout, SVG, pan/zoom)
src/features/<screen>/   pairing, queue, graph, item (Details / Work / Agents),
             inbox, chat, commands, overview (spend / relationships), new-item, settings
dev/         dev server and WebSocket bridge to a real archond (not shipped)
tests/       Playwright flows, unit tests and private archond helpers (not shipped)
scripts/     e2e, screenshot, icon and entity generators (not shipped)
```

Overview is the landing screen. Five destinations remain visible in a phone's
bottom bar; at 900 px the bar becomes a left rail. Settings is in the phone
header and desktop rail. Queueing work, answering questions and pausing dispatch
remain direct controls. Chat's Commands sheet exposes the host's operator
commands, including its existing confirmations and observer-role refusals.

Graph displays `task_graph` dependencies, stack edges and initiatives, with
repository, initiative, text and history filters. Active work takes priority
within the 500-item cap, followed by work finished in the last seven days.
Choose Active only or All history to change that window. When the cap is
reached, the view states that items and connections were omitted. Edges point from prerequisite to dependent; kinds and satisfaction
are labelled as well as styled. Phone layouts flow vertically. Drag/pinch or
use the zoom controls; Tab reaches nodes, Enter opens details, arrows pan and
`+`, `-`, `0` zoom or fit. The default camera preserves readable text; Fit shows
the entire graph. Cycle and missing-endpoint diagnostics stay visible.

An item's Work view combines frozen `run_plan` stages with actual attempts,
fan-out slots and gates. Agents preserves the recorded delegation tree and
uses the bounded read-only `item_agent_states` request for live/completed/stopped
states (including the current orchestrator). Each node keeps the cost of its own
attempt: an agent's lifetime spend there would repeat on every attempt. Backend
failures remain visible; unavailable states are never inferred from a parent's
state. Selecting a stage opens its recorded output for that exact run/attempt/slot.
Overview's spend chart has an equivalent daily table, and relationships link
initiatives across repositories and pull requests. Item details retain merge history.

Motion uses only transform and opacity. Reduced motion disables CSS animation
and transitions, cancels ongoing list/graph movement, and leaves every control
usable. Graph layouts are iterative and bounded; camera frames update one SVG
group without layout reads. No graph request changes pairing or mutation authority.


What ships is everything under `web/` except `dev/`, `tests/`, `scripts/`,
`node_modules/`, `test-results/`, `playwright-report/`, hidden files,
`package*.json`, `*.md`, `*.config.js`, `*.test.js` and `*.spec.js`. The rule
lives in `dev/shipped.js`; a unit test keeps the service worker's precache
list in step with it and the total under 1 MB.

## Run it against a real archond

```sh
npm ci
ARCHON_SOCKET=/path/to/archond.sock npm run dev       # http://127.0.0.1:8080
```

`dev/server.js` serves the shipped files with the production CSP, stubs
`/api/health`, `/api/session` (always paired as "Dev browser"),
`/api/pair/claim` (code `DEVCODE1`) and `/api/session/forget`, and bridges
`/ws` to the archond Unix socket. It speaks to archond as the CLI client and
refuses any command outside the web allowlist with `forbidden`.
Pass `--require-pairing` to make the browser pair first, and `--port N` to move it.

## Tests

The browser tests start their own archond in a temp home (Aether stays down)
and need the `archon` binary: set `ARCHON_BIN`, or `CARGO_TARGET_DIR` for
`$CARGO_TARGET_DIR/debug/archon`.

```sh
npm ci
node --test                    # unit tests: transport, pairing code, bridge, shipped files, Markdown
npx playwright test            # every flow against archond's own listener, and the dev bridge
npm run e2e                    # both, installing Chromium first
npm run screens                # regenerate docs/assets/remote/*.png
```

`npm run e2e` (`scripts/e2e.js`) runs `npm ci` when `node_modules` is
missing and `npx playwright install chromium`. When Chromium cannot be
installed it prints SKIP and exits 0, unless `CI` is set, where it fails.

The flows in `tests/flows.js` take `(page, fixture)`, where
`fixture = { baseURL, archond, pairCode(): Promise<string> }`, so the same
flows run against both: `tests/listener.spec.js` uses archond's own listener,
set up and paired with `archon remote` (`tests/support/listener.js`), and
`tests/dev.spec.js` the dev bridge, which can also drop or revoke a socket.
`archond` comes from `tests/support/archond.js`: `startArchond({ archonBin })`
gives `{ home, socket, env, protocolVersion, run(args), request(cmd), stop() }`.
Each test fails on any console error, page error or CSP violation.

Screenshots: `npm run screens` captures phone (390×844) and desktop (1280×800),
both themes. Pipeline/agent captures use explicit Aether response fixtures; the
queue and other controls use a private real daemon. `tests/screens.js` exports `captureScreens(page, fixture, outDir)`,
which writes `<screen>-<light|dark>.png` for pairing, queue, item, inbox,
chat, overview, new-item, settings, graph, work, agents and commands at the page's viewport.

## Live design sessions

Route `#/design/<item>` (`src/features/design/`), shown only when the daemon's
welcome features include `design_sessions`. The screen sends `design_get`, then
`subscribe_design`, draws the latest frame into a canvas, acks frames, and
restarts the stream on reconnect. Modes: Interact, Comment, Region and Sketch;
notes are queued offline with a stable `client_key`; Compare, Finish review and
the gate answer live in the same screen. Entry points: the New item "Design it
live" switch, the item Design chip / Open design, and the inbox "Design session
ready" Open.

Modules: `state.js` (reducer, `stagePhase`), `stream.js`, `input.js`,
`scale.js`, `keymap.js`, `overlay.js`, `composer.js`, `notes.js`, `compare.js`,
`finish.js`, `sketch.js`, `outbox.js`, `chrome.js`, `screen.js`; the wire
commands and the stream hub are in `src/core/design.js`.

Tests: `tests/unit/design-*.test.js`, and `tests/design.spec.js` (T-E1..T-E4),
which runs against a stand-in host and a scripted WebSocket daemon
(`tests/support/design-harness.js`) with the fixture app in
`tests/fixtures/design-app/`. Playwright needs Node 20+.
