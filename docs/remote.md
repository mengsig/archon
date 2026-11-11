# Remote access: Archon on your phone

Archon's mobile web app (the queue, each item's stages and verdicts, the
operator chat, the questions and gates waiting on you, and the actions you
take every day) is embedded in archond. Aether's listener forwards
`/archon/…` to archond's listener, the `/archon` mount, so the phone opens
`https://<origin>/archon/`; archond serves the app, the API, the pairing and
the WebSocket behind the mount. The app uses relative URLs, so it works at
the mount and at the root of archond's own listener. This page is the reference: what archond serves, how a device is paired
and authenticated, and what a paired device may do. To set it up, follow
[Use Archon from your phone](remote-setup.md).

Remote access is **off** until you switch it on. When on, archond listens on
loopback only (`127.0.0.1:8378`); `tailscale serve` publishes Aether's
listener, the one origin, and Aether forwards to archond. **Pairing a device as an operator (the default) gives it your full
operator power**: from the app it does what you do from the TUI, including
`/archon` commands, `/answer`, `/approve`, `/merge`, directives and settings.
Pair a phone others may hold as an observer, and revoke a lost device at once
(`archon remote revoke`). Each request is checked against a fixed list
([what a device may do](#what-a-device-may-do)) and then handled exactly as
the same request from your `archon` CLI, so archond's own rules (the
autonomy ceiling, shared mode) apply unchanged.

## Contents

- [Threat model](#threat-model)
- [Configuration](#configuration)
- [Pairing](#pairing)
- [HTTP endpoints](#http-endpoints)
- [The WebSocket](#the-websocket)
- [What a device may do](#what-a-device-may-do)
- [Revoking and switching off](#revoking-and-switching-off)
- [Commands](#commands)

## Threat model

What remote access protects, and from whom:

- **Someone who can reach the address but holds no credential** (another
  device on your tailnet, a local user on this machine, a browser tab on a
  hostile site). They get the app's static files (no data in them) and
  `GET /api/health`, nothing else. Every API call and the WebSocket need a paired device's
  credential. Pairing needs the one-time code that only `archon remote
  pair` (or `setup`) shows on your terminal.
- **Script on a shared origin.** Behind the mount the app runs on Aether's
  origin, so a script injected into it would reach Aether's same-origin API.
  The app's `script-src 'self'` policy, which Aether passes through, and its
  Markdown renderer (no raw HTML, only http(s) and mailto links) guard
  against that; its service worker deletes only `archon-shell-*` caches.
- **Guessing the code.** It is 8 symbols of Crockford base32 (40 bits), open
  for 5 minutes, used once. Guesses are limited to 3 per 10 seconds across
  all clients (`429` with `Retry-After`), and the fifth wrong code closes
  the pairing. In 5 minutes at most 90 guesses are possible, a chance of
  about 1 in 10¹⁰.
- **Cross-site requests and DNS rebinding.** Every request's `Host` must be
  an origin archond was told about (the public origins you added, or
  loopback on the listening port), so a hostile name pointed at
  `127.0.0.1` is refused. Mounted, `Host` must be loopback, as Aether's
  mount sends it, and `Origin` one of `trusted_origins`, Aether's own. A `POST` and the WebSocket upgrade need an
  allowed `Origin` and, when the browser sends it, `Sec-Fetch-Site:
  same-origin`. Bodies must be `application/json`, which a cross-site form
  cannot send. The credential cookie is `HttpOnly` and `SameSite=Strict`.
- **A stolen credential.** Each device has its own 256-bit secret. archond
  stores only its SHA-256, in `remote.json` (mode 0600) under its data
  directory. `archon remote revoke` forgets the device and closes its open
  sessions at once (WebSocket close code 4401).
- **A paired device doing more than you could.** It cannot: requests outside
  the [list](#what-a-device-may-do) are refused with `forbidden`, and the
  rest go through archond's operator checks as the CLI's do. An operator
  device has exactly your authority from the TUI; an observer's chat lines
  carry none. Caps, promotions, repository removal, gc, upgrade requests and
  remote access itself stay on the CLI. A merge the orchestrator proposes
  still waits for the confirmation question it always asks, which you may
  answer from the app as from the chat.
- **A lost or stolen operator device.** It acts as you until you revoke it:
  `archon remote revoke` refuses its next request and closes its sessions.
- **Secrets in URLs and logs.** The pairing code is typed on the Aether
  app's Archon pairing screen and never put in a link. No credential is ever
  in a URL. archond logs device ids, never codes or credentials.
- **Exposure.** The listener binds loopback unless you set
  `allow_remote = true`, and serves plain HTTP: TLS is the proxy's job.
  The setup wizard publishes only Aether's listener, only to your tailnet
  with `tailscale serve`, never with Funnel, and stops while Funnel
  publishes anything on this machine; Aether refuses to mount then too.

What it does not protect against: anyone who can run commands as you on
this machine (they can use the CLI and read the data directory anyway), a
compromised phone while it is paired, and a proxy you configure to forward
someone else's traffic.

## Configuration

Static settings live in `config.toml`; a change needs an archond restart.

```toml
[remote]
address = "127.0.0.1:8378"   # where the listener binds
allow_remote = false         # true to bind a non-loopback address
max_connections = 16         # HTTP connections served at once (1 to 1024)
mounted_at = "/archon"       # set by `archon remote setup`: served behind Aether's mount
trusted_origins = ["https://box.tail1234.ts.net"]  # Aether's public origins, at most 16
```

`mounted_at` and `trusted_origins` come together: with them, archond serves
behind Aether's mount, its cookie is scoped to the prefix, and it answers
only loopback `Host` values and the trusted `Origin` values. `archon remote
setup` writes them from Aether's public origins and asks for a restart.

`allow_remote = true` lets the listener bind a non-loopback address. The
listener still speaks plain HTTP and still checks `Host` and `Origin`; use
it only on a network you trust, behind a TLS proxy.

What changes at run time is kept in `remote.json` in archond's data
directory, mode 0600, written through a private temporary file. Its
`version` is 1 while every device is an operator, which Archon 1.4 also
reads, and 2 once an observer is paired:

| Field | Meaning |
| --- | --- |
| `enabled` | Whether the listener opens when archond starts. |
| `public_origins` | The `https://host[:port]` origins browsers may use besides loopback, at most 16. |
| `devices` | The paired devices: `id`, `name`, `secret_sha256`, `paired_at`, `last_seen_at`, and `role` in format 2. At most 64. |

A `remote.json` that does not parse stops archond with the path and the
fix: move it away to reset remote access. An open pairing code lives only
in archond's memory and never outlives it.

## Pairing

1. `archon remote pair` (or `archon remote setup`) opens a pairing and
   prints its code (`7K2M-9QXD`) and every address it works at; setup also
   shows a QR code of the app's pairing screen,
   `https://<origin>/archon/pair`. A new pairing replaces any open one.
2. The browser opens that screen, you type the code and name the device,
   and it sends `POST /archon/api/pair/claim`, which Aether forwards to
   archond's `POST /api/pair/claim`.
3. archond checks the code (case, dashes and spaces ignored; `O` reads as
   `0`, `I` and `L` as `1`), closes the pairing, stores the new device, and
   answers with its credential in a cookie.

The code must be claimed within 5 minutes. A wrong code counts towards the
5 that close the pairing; an expired or closed pairing needs a new
`archon remote pair`.

## HTTP endpoints

Paths are archond's own; through Aether's mount each is under `/archon`.
Every response carries `Content-Security-Policy: default-src 'none';
style-src 'sha256-…'; …; frame-ancestors 'none'` (no script runs), `X-Content-Type-Options:
nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`,
`Cross-Origin-Opener-Policy` and `Cross-Origin-Resource-Policy: same-origin`,
and a `Permissions-Policy` that turns off the camera, microphone and geolocation.
API responses are `Cache-Control: no-store`.

Checks, in order: the `Host` (403 `forbidden_host`), the path (400 on empty,
`.` or `..` segments, backslashes or `%` escapes), the method (405), then
the endpoint's own. Request heads must arrive within 10 seconds and be at
most 32 KiB; bodies at most 16 KiB (413) within 10 seconds. Errors are
`{"error": {"code", "message"}}`.

| Endpoint | Needs | Answers |
| --- | --- | --- |
| `GET /api/health` | — | `{"status":"ok","pairing_open":bool,"version"}` |
| `POST /api/pair/claim` | Origin, JSON `{"code","device_name"}` | `200 {"device":{id,name,paired_at}}` and `Set-Cookie`; `403 invalid_code` or `pairing_closed`, `410 expired`, `429 rate_limited` |
| `GET /api/session` | the cookie; same-site | `{"device","protocol_version","daemon_version"}`; `401` (and a cleared cookie) when not paired |
| `POST /api/session/forget` | Origin, the cookie | unpairs this device and clears the cookie |
| `GET /ws` | Origin, the cookie, subprotocol `archon` | the WebSocket |
| `GET /…` | — | the embedded app: the file named, or `index.html` for a path with no extension (`/pair` is the pairing screen); `404` for a missing file. Every response carries a strict Content-Security-Policy (own origin only) and `no-cache` with a strong `ETag` |

The cookie is `archon_device=<id>.<secret>; Path=/; HttpOnly;
SameSite=Strict; Max-Age=34560000`, plus `Secure` when paired over HTTPS;
mounted, `Path` is the mount (`/archon`), so Aether's own cookie and
Archon's never meet.
`device_name` is 1 to 64 characters, whitespace folded; control,
invisible, bidirectional and private-use characters are refused.

## The WebSocket

`GET /ws` upgrades to a WebSocket with the subprotocol `archon` (version
13 only). It carries archond's own protocol
([ARCHITECTURE §10](ARCHITECTURE.md)), one JSON frame per text message: the
first message must be `hello` with protocol 23 or 24 (archond's own is
24), and archond answers `welcome`, which names the device and its role at
24. After that each `{"req": N, "cmd": …}` is
answered by a `reply` frame with the same id, and `subscribe_feed` streams
`feed` frames, as on the Unix socket.

Limits: messages of at most 1 MiB, 16 requests in flight and 4 feed
subscriptions per connection, and as many sessions in total as
`max_connections`. archond pings every 25 seconds and closes a session
silent for 75.

| Close code | Meaning |
| --- | --- |
| 4401 | This device was revoked; pair again. |
| 4403 | Remote access was switched off. |
| 4002 | An allowed origin was withdrawn; the app reconnects and is checked again. |
| 1001 | archond is stopping. |
| 1008 | A protocol violation (no `hello` first, a wrong version). |
| 1009 | A message over 1 MiB. |

## What a device may do

A device is an **operator** (the default, and every device paired before
1.5.0) or an **observer**, paired with `archon remote pair --observer`. The
role is the one archond stored when the device paired; nothing the browser
sends, and no line of text, changes it. An observer sends only the reads; an
operator sends both columns:

| Reads | Changes (operators only) |
| --- | --- |
| `status`, `repo_list`, `list_items`, `list_item_summaries`, `get_item`, `get_stage`, `resolve_item`, `list_pipelines`, `model_policy`, `get_initiative`, `list_questions`, `feed_history`, `subscribe_feed`, `list_reports`, `orchestrator_status`, `cost_report`, `list_artifacts`, `read_artifact`, `item_diff`, `settings`, `autonomy`, `subscribe_changes`, `changes_page`, `item_history`, `task_graph`, `run_plan`, `item_links`, `issue_items`, `list_github_objects` | `enqueue`, `steer_item`, `update_item`, `cancel_item`, `retry_item`, `pause`, `resume`, `answer`, `say`, `operator_command`, `merge_item`, `set_directive`, `clear_directive`, `set_settings`, `reset_settings`, `repo_add`, `repo_update`, `configure_github_watcher`, `link_issue`, `unlink_issue`, `import_issue` |

An operator device's request runs with the operator CLI's standing, so it
may set `merge=auto` or `tools=yolo` within the autonomy ceiling, pause or
resume hard, and confirm an authority change with `answer`. A chat line from
it reaches the orchestrator as your TUI's does: it is your line, and the
orchestrator may cite it for `request_authority` (not in shared mode, which
refuses chat citations to everyone). A line that is a host command
(`/archon`, `/answer`, `/approve`, `/reject`, `/merge`, `/watch`,
`/upgrade`, `/update`) runs as the TUI runs it, as an `operator_command`,
never as a chat line, so it works while Aether is down and in shared mode as
the CLI's does. For example, `/archon apply {"cmd":"set_directive",
"text":"docs fixes only","expires_in_hours":null}` sets a directive and
`/archon apply {"cmd":"clear_directive","directive":null}` clears them.

An observer only reads: the listener refuses its `say` and `answer` before
archond sees them. Behind that, archond still gives an observer's requests
the Aether app's standing (`archon-web`), whose relayed lines go out marked
as chat, never operator authority.

archond logs every change a device asks for with the device's id, name and
role, and whether it applied. Host administration (`shutdown`, `upgrade`,
`gc`, `remote_*`, orchestrator restarts), caps, `repo_remove`, `promote`
and every agent-side request stay with the CLI on the host; they and a
second `hello` are refused with `forbidden` and the message "… is host
administration, which only the archon CLI on the host does". A change from
an observer gets "… changes Archon. This is an observer device: it can
only read. Pair it as an operator to act from it.", and a request a revoked
device had already sent gets "… refused: this device was unpaired or remote
access was turned off".

A device Aether paired (Aether's own app) is Aether's business: its chat
line is an operator's line for Archon only when Aether's session stamps the
device's operator role on it (`origin.role: "operator"`). An Aether that
stamps no role, or a device Aether paired as an observer, confers nothing.

## Revoking and switching off

- `archon remote revoke <id or name>` forgets one device and closes its
  sessions at once; its cookie no longer works, and a request it had
  already sent is refused. Revoke a lost or stolen phone at once: an
  operator device holds your full operator power. "Forget this device" in the
  app does the same for the browser in your hand. Aether's device for the
  same browser is Aether's to revoke (`archon aether remote revoke`).
- `archon remote disable` closes the listener, every session (4403) and any
  open pairing. Paired devices are kept, and work again after `archon
  remote enable`.
- `archon remote origin remove` and `archon remote setup` close every open
  session (4002) when they withdraw an origin, since origins are
  checked when a session opens (and again once it is registered, so an
  upgrade racing the withdrawal is refused); sessions from origins still
  allowed reconnect.
- A `tailscale serve` that still points at a closed listener answers `502`.
  Remove it with `tailscale serve --https=<port> off`.

## Commands

| Command | Does |
| --- | --- |
| `archon remote` (`status`) | Whether the listener is on, its origins, and the paired devices. |
| `archon remote setup` | Mount archond on Aether and publish Aether's listener ([remote-setup.md](remote-setup.md)). |
| `archon remote enable` / `disable` | Open or close the listener. |
| `archon remote pair` | Open a pairing code and show its QR code; `--observer` pairs a device that only reads. |
| `archon remote devices` | The paired devices. |
| `archon remote revoke DEVICE` | Unpair one device. |
| `archon remote origin add URL` / `remove URL` | Allow or drop an `https://` origin. |

Every command takes `--json`. archond serves them as operator-only
requests (`remote_status`, `configure_remote`, `remote_pair`,
`remote_revoke`), which neither the MCP proxy nor a paired device may send.
