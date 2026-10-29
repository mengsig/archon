# Archon web app

The phone (and desktop) app that the remote listener serves: watch the queue,
answer questions, chat with the orchestrator, queue work and pause Archon.
Plain ES modules and CSS, with no framework, no build step and no runtime
dependencies. It runs under a strict CSP (no inline script or style, no eval).

## Layout

```
index.html, manifest.webmanifest, sw.js, icons/   the installable shell
styles/      tokens.css (themes), base.css, components.css, screens.css
src/core/    transport (WebSocket, reconnect, req-id correlation), api (HTTP),
             daemon (request shapes), feed, store, router, format, theme
src/ui/      DOM helper, components, sheets, toasts, list reconcile, motion,
             Markdown (markdown-parse.js: tree; markdown.js: DOM via h())
src/features/<screen>/   pairing, queue, item, inbox, chat, overview,
             new-item, settings
dev/         dev server and WebSocket bridge to a real archond (not shipped)
tests/       Playwright flows and specs, unit tests, archond helpers (not shipped)
scripts/     e2e runner, screenshot and icon generators (not shipped)
```

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

Screenshots: `tests/screens.js` exports `captureScreens(page, fixture, outDir)`,
which writes `<screen>-<light|dark>.png` for pairing, queue, item, inbox,
chat, overview, new-item and settings at the page's viewport.
