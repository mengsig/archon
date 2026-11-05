# vNext security audit: Archon (H3)

Task H3 of mengsig/archon#377, filed as mengsig/archon#388. This is the
Archon half. The Aether half is audited on mengsig/aether#414.

- **Scope.** archond's remote listener behind Aether's `/archon` mount
  (AR-R, AR-L), Archon device roles and operator standing (`/archon` chat
  commands, `request_authority`, the GitHub watcher's operators), and the
  protocol 24 GitHub surface (D1–D3: links, object cache, import, outbox).
  ARCHITECTURE.md §c (Electron) is checked where it calls archond. Every
  other §c item is marked "Aether side".
- **Base.** Archon `f1dbfc2b` (the head of integration PR #378 when this
  item was queued). Aether read-only at `84d897d`, branch
  `archon/2t52sdk3-vnext-memory-2-0-step-1-reproducible-ben`.
- **Method.** Read the code, cite file and line or a test, and add a test
  where a claim was cheap to test and had none. Paths are relative to the
  repository root. `aether:` marks a path in the Aether repository.
- **Severity.** *High*: a remote or observer role can read secrets or
  sealed data, write, run or escalate beyond its class, or a renderer can
  reach native power it should not. *Medium*: a defence named in §c/§d is
  missing, but another layer still blocks the attack. *Low*: hardening.

## Findings

| Id | Severity | Area | Status |
| --- | --- | --- | --- |
| H3-A1 | high | operator standing / remote roles | fixed (see [Fixes](#fixes)) |
| H3-A2 | high | operator standing / shared origin | fixed (see [Fixes](#fixes)) |
| H3-A3 | low | GitHub outbox | follow-up |
| H3-A4 | low | GitHub cache (ETag) | follow-up |
| H3-A5 | low | remote roles (authority confirmations) | follow-up |
| H3-A6 | low | `archon remote setup` | follow-up |
| H3-A7 | low | Aether `[listen]` token clients in dedicated mode | follow-up |
| H3-X1 | high | Electron bridge (Aether) | for the other repository |
| H3-X2 | low | Electron allowlist drift (Aether) | for the other repository |

### H3-A1 (high): a web device's `say` ran as verified operator chat

**Scenario.** An Archon device paired as `operator` sends
`{"cmd":"say","text":"/archon resume --hard"}` over `/ws` (or
`/archon watcher {...}`, `/archon set …`, `/archon apply {...}`,
`/upgrade …`, `/merge …`). The listener allows `say` for operators
(`crates/archon-remote/src/allow.rs:256-298`). archond echoes the line to
the feed and sends it to the orchestrator over its own Aether socket
(`crates/archon-daemon/src/handlers/operator.rs:26-44` at base `f1dbfc2b`,
`crates/archon-daemon/src/supervisor.rs:713-731`). Aether stamps every
local-socket send as `Actor::User {origin: local_socket}`
(`aether: crates/aether-daemon/src/authz.rs:91-110`). Archon then reads the
delivery as Aether chat from the operator
(`crates/archon-aether/src/events.rs:248-283` at base). In dedicated mode
`crates/archon-daemon/src/pump/orchestrator.rs:97-100` hands it to
`crates/archon-daemon/src/operator_commands.rs:15-51`. That code records the
line as citable evidence and runs the command. The device has lifted a hard
pause, which AR-R refuses to devices (BASELINE.md §3.2). It can equally change
the watcher's operator logins, loosen ceilings, merge or upgrade, all of which
are CLI-only for every device (`docs/remote.md`, "A paired device doing more
than you could"). A prose line works too: the orchestrator can cite it through
`request_authority` (`crates/archon-daemon/src/authority/evidence.rs:48-73`).

**Fix.** A web `say` reaches the orchestrator prefixed with
`archon_core::authority::WEB_RELAY_MARK`. The event parser gives no operator
channel to a delivery that starts with the mark. With no channel, no command
runs and nothing is recorded as citable. Downgrading on the mark is sound:
archond adds it itself, and a line that carries it can only lose authority,
never gain it. The orchestrator still gets the line, and the app still sees
its reply. Approvals from the app still go through `answer`.

### H3-A2 (high): an Aether-paired device's chat carried Archon operator authority

**Scenario.** On the shared origin, a browser is paired with Archon's
dedicated aetherd as an Aether `operator` device. Its Archon device may be an
`observer`, or it may hold no Archon device at all. It sends a message to the
orchestrator agent. Aether records `Actor::User {origin: {transport: device}}`
(`aether: crates/aether-daemon/src/authz.rs:99-108`). Archon's parser read
only `/from/actor == "user"` and called the line `AetherChat`
(`crates/archon-aether/src/events.rs:259-275` at base). Every command and citation in
H3-A1 then ran for that device. This breaks ARCHITECTURE §d: "each daemon
keeps its own pairing, device identity, roles". It also breaks "a request …
gets exactly the Archon role of its credential and nothing from the Aether
pairing".

**Fix.** Only the host's own Aether connections speak with operator
authority. That means a `user` actor whose origin is absent (Aether before
origins), `local_socket` or `tcp` (a host-issued `[listen]` token, see
H3-A7). A `device` origin, or any transport Archon does not know, gets no
channel. Aether's observers already appear as `client` actors and stay
refused. `docs/operator-authority.md` now says that the Aether remote app
carries chat, not authority.

### H3-A3 (low): the outbox accepts any author's marker

`find_comment` (`crates/archon-daemon/src/github/outbox.rs:398-429`) marks a
pending row posted when *any* comment on the thread holds its marker. It does
not check that the comment's author is Archon's `gh` login. The token is
random (`obx_…`, `crates/archon-core/src/id.rs:124`), but observers can read
pending tokens in `status.github_outbox`. Someone who reads one and can
comment on the thread can suppress that one reply or ship comment. That
cannot cause a double post and grants nothing. **Follow-up:** match
`user.login` against the repository's `gh` login before accepting a marker.

### H3-A4 (low): a cached ETag goes back to `gh` unchecked

`read_args` (`crates/archon-daemon/src/github_sync/fetch.rs:31-47`) passes
the stored ETag as `-H If-None-Match: {etag}`. It is one argv entry, never a
shell, but it is GitHub's own header value stored verbatim. **Follow-up:**
refuse an ETag with control characters when it is cached.

### H3-A5 (low): an operator device may confirm an authority change

An operator device's `answer` can say `yes` to an authority confirmation
(`crates/archon-daemon/src/authority/confirmations.rs:152-200`). That applies
a change that a verified channel (CLI, dedicated chat or GitHub operator)
requested. The device starts nothing itself: with H3-A1 fixed, no device line
can request authority. Answering is documented for devices (`docs/remote.md`,
"What a device may do"). **Follow-up:** decide whether authority
confirmations, unlike gate and merge questions, should be answered only from
the CLI or verified chat.

### H3-A6 (low): `archon remote setup` reads Tailscale, not `mounts_blocked`

§d says setup "checks `get_remote` before it adds the mount, and refuses with
the same reason". Setup reads Tailscale itself and refuses any Funnel with
Aether's reason word for word (`crates/archon/src/cmd/remote/setup.rs:166-173`,
`crates/archon/src/cmd/remote/setup_publish.rs:18-21,55-75`). Aether also
refuses the add (`ensure_mount`, `crates/archon/src/cmd/remote/setup_aether.rs:184-226`).
The defence holds through both layers, but setup does not read Aether's own
`mounts_blocked: "status_unknown"`. **Follow-up:** read `mounts_blocked` from
`aether remote status --json` as well.

### H3-A7 (low): `[listen]` token clients keep chat authority in dedicated mode

A `tcp` origin is Aether's host-issued `[listen]` token, not a pairing. It
keeps today's dedicated-mode chat authority, and shared mode refuses it
(`crates/archon-daemon/src/operator_commands.rs:175-180`).
**Follow-up:** decide whether a token client should be a verified chat
channel at all.

## For the other repository

### H3-X1 (high, Aether): the desktop bridge greets archond as `archon-cli`

`aether: web/apps/desktop/src/bridge/bridge.ts:122` sends
`hello {client: "archon-cli"}` on archond's socket. The renderer's `request`
frames that pass `ARCHON_PASS`
(`aether: web/apps/desktop/src/security/classes.ts:58-65`) then run with CLI
standing. A soft `{"cmd":"resume","scope":"global"}` passes the bridge's
`hard` check (`classes.ts:142`). archond, though, refuses to release the
operator's *hard* pause only for `archon-mcp` and `archon-web`
(`crates/archon-daemon/src/handlers/control.rs:488-497`). So a compromised
`app://` renderer lifts a hard pause, which is CLI-only (`OperatorChange`)
power, and stage agents resume. The same standing also records the
renderer's changes as `by: "cli"` in item history
(`crates/archon-daemon/src/handlers/changes.rs:82`). **Remedy:** greet with
`client: "archon-web"`, which archond accepts at 23..=24
(`crates/archon-core/src/protocol.rs:122-127`). archond's own web refusals
then apply on top of the bridge's filter. Keep `archon-cli` only for the
`privileged` actions, which main builds and confirms itself. Archon cannot
tell the bridge from the CLI on its socket (`ClientKind` is self-declared,
`crates/archon-core/src/protocol.rs:190-201`), so the fix belongs to Aether.

### H3-X2 (low, Aether): the bridge's Archon allowlist is a hand copy

`ARCHON_PASS` is copied from `allow.rs` "at Archon 9a8d63d1". Archon's schema
does not publish `Request::class()` or the device allowlist
(`crates/archon-core/src/protocol/schema.rs:14-31`), so a new Archon request
cannot reach the bridge by mistake, and a changed class is not caught
either. **Remedy:** a test that diffs `ARCHON_PASS` against a list Archon
publishes. On the Archon side, emitting each command's class into
`protocol.schema.json` is the follow-up.

## ARCHITECTURE §d as it applies to archond

| §d item | Status | Evidence |
| --- | --- | --- |
| Browser access through one origin, Aether's listener; archond keeps its own pairing, device identity, roles, Host/Origin checks and revocation | ✅ | Pairing `crates/archon-remote/src/pairing.rs:69-120`; devices and SHA-256 secrets `crates/archon-remote/src/devices.rs:29,56`; roles `crates/archon-remote/src/allow.rs:226-238`; checks `crates/archon-remote/src/http/checks.rs`; revocation `crates/archon-remote/src/lib.rs:291` |
| The mount adds no credential and no standing | ✅ after H3-A2 | archond authenticates only its own `archon_device` cookie (`crates/archon-remote/src/http/api.rs:111-137`). Aether-device chat no longer confers Archon authority (test `an_aether_paired_device_confers_no_archon_authority`) |
| archond trusts no header a browser could set to choose identity or standing | ✅ | archond reads only `Host`, `Origin`, `Sec-Fetch-Site`, `Cookie` (`archon_device` only), `Content-Type` and the WebSocket upgrade headers (`grep -rn 'header::' crates/archon-remote/src`). There is no `X-Forwarded-*` (`grep -rni forwarded crates/archon-remote/src` prints nothing). `Origin` only narrows what is accepted. Identity comes from the cookie alone |
| A mounted request gets exactly the Archon role of its credential | ✅ | Role from the cookie's device at upgrade (`crates/archon-remote/src/ws.rs:31,46-50`), checked per frame (`crates/archon-remote/src/ws/session.rs:267-270`), passed to archond (`session.rs:289`). archond asserts the allowlist and runs as `ArchonWeb`, whatever the `hello` says (`crates/archon-daemon/src/remote.rs:30-41`). Tests `an_observer_reads_and_is_refused_every_change`, `the_socket_carries_allowed_requests_and_refuses_the_rest` (`crates/archon-remote/tests/listener.rs:644,465`) |
| Mounted, archond accepts the mount's origins only when told it is mounted, and scopes the cookie to `Path=/archon` | ✅ | `crates/archon-remote/src/http/checks.rs:31-36`, cookie path `crates/archon-core/src/remote.rs:87-89`, `crates/archon-remote/src/http/api.rs:146-153`. Tests `mounted_the_cookie_is_scoped_to_the_mount_and_only_trusted_origins_are_accepted`, `mounted_an_upgrade_from_loopback_is_refused` (`listener.rs:698,761`), unit tests `mounted_the_host_is_only_loopback_even_on_a_non_loopback_bind`, `mounted_the_host_is_local_and_only_trusted_origins_are_accepted` |
| `trusted_origins` at most 16, `https://` or loopback `http://`, set with `mounted_at` | ✅ | `crates/archon-core/src/remote.rs:93-131`; tests `trusted_origins_are_https_or_loopback_http`, `a_mount_needs_a_prefix_and_trusted_origins_together` |
| Path handling under the stripped prefix | ✅ | `check_path` refuses `.`, `..`, empty segments, `%` and `\` (`crates/archon-remote/src/http/checks.rs:137-150`, test `traversal_and_escapes_are_refused`). archond serves no files: a file-like path is 404 (`crates/archon-remote/src/http.rs:149-157`). `/api/*` is routed exactly (`http.rs:107-144`) |
| CSRF for the served app | ✅ | POSTs and `/ws` need an allowed `Origin` and `Sec-Fetch-Site` of `same-origin` or none (`checks.rs:74-90`). Bodies must be `application/json`, at most 16 KiB (`checks.rs:165-212`). The cookie is `HttpOnly; SameSite=Strict` (`api.rs:146-153`). Tests `a_code_pairs_one_device_once_and_cross_site_claims_are_refused`, `an_upgrade_needs_the_cookie_the_origin_and_the_subprotocol` |
| Host anti-rebinding | ✅ | Exactly one `Host`, an allowed authority, and an absolute URI must agree (`checks.rs:53-70`). Test `hosts_must_be_ours_and_single`; `health_and_the_pointer_page_are_served_with_security_headers_to_known_hosts_only` |
| CSP and security headers | ✅ | `default-src 'none'`, hashed style, `frame-ancestors 'none'` (`crates/archon-remote/src/assets.rs:22-33`). `nosniff`, `DENY`, `no-referrer`, COOP/CORP on every response (`crates/archon-remote/src/http.rs:174-197`). The pointer page escapes the origin (`assets.rs:76-89`) |
| Device `role` chosen by the host when pairing | ✅ | `remote_pair {role}` is `OperatorChange` (`crates/archon-core/src/protocol.rs:828`). The role is fixed in the pairing (`pairing.rs:23-24,69-97`) and copied to the device on claim (`api.rs:67-69`) |
| `remote.json` format version; old files load as `operator` | ✅ | `crates/archon-remote/src/store.rs:5-110` (format 1 has no roles, format 2 names each) |
| Allowlists split per role; observers only read | ✅ | `crates/archon-remote/src/allow.rs:226-238`; tests `an_observer_is_refused_every_change_and_reads_the_rest`, `daily_operation_is_allowed_to_operators` |
| `pause`/`resume` with `hard:true` refused for devices (BASELINE §3.2) | ✅ after H3-A1 | `allow.rs:245-251`, tests `a_hard_pause_or_resume_is_refused_to_every_role`, `a_hard_pause_or_resume_is_refused_and_a_soft_one_reaches_archond`. archond refuses a web release of a hard pause (`crates/archon-daemon/src/handlers/control.rs:493`, test `a_web_device_cannot_lift_the_operators_hard_pause`). The chat bypass (`say "/archon resume --hard"`) is H3-A1, closed by test `a_web_devices_say_carries_no_operator_authority` |
| New read models observer-visible; new mutations operator-only | ✅ | `link_issue`, `unlink_issue` and `import_issue` are `Change` and refused to observers (`allow.rs` test `an_observer_is_refused_every_change_and_reads_the_rest` covers all 11 changes). The protocol 24 reads are `Read` (`protocol.rs:749-791`) |
| Web requests run with `ClientKind::ArchonWeb` standing | ✅ | `crates/archon-daemon/src/remote.rs:40`; refusals keyed on it at `handlers/control.rs:493`, `handlers/changes.rs:82` |
| Observer limits | ✅ | Reads expose no credential. Device views carry no secret hash (`crates/archon-core/src/remote.rs:167-177`, `api.rs:139-143`), `remote_status` is `OperatorRead` and refused to every device (`allow.rs` test `administration_merges_and_agent_requests_are_refused`). Artifact names are plain relative paths (`crates/archon-daemon/src/artifacts.rs:53`, test `stored_names_are_plain_relative_paths`). Pending outbox tokens are observer-visible (H3-A3) |
| Revocation closes sessions at once (4401); disable (4403); origin withdrawal (4002) | ✅ | `crates/archon-remote/src/sessions.rs:19-24`; re-checked after registration (`ws.rs:74-92`). Tests `revoking_a_device_closes_its_session_at_once_and_disabling_closes_all`, `withdrawing_an_allowed_origin_closes_open_sessions`, `an_upgrade_that_registers_after_its_origin_was_withdrawn_is_refused`, `a_revoke_lands_while_the_device_floods_requests_it_does_not_read` |
| Pairing codes: one-time, 5 min, rate-limited | ✅ | `pairing.rs:3-61`; test `guesses_are_rate_limited_and_the_store_is_private` |
| Funnel refusal; `archon remote setup` refuses before mounting | ✅ (H3-A6 low) | `crates/archon/src/cmd/remote/setup.rs:166-173`, `setup_publish.rs:55-75`; Aether also refuses the add (`setup_aether.rs:184-226`) |
| archond listens on loopback when mounted | ✅ | Setup refuses a non-loopback listener (`setup.rs:224-229`). Mounted `Host` is loopback only, whatever is bound (`checks.rs:31-36`) |
| Private by default | ✅ | `allow_remote = false` and the listener is off until enabled (`docs/remote.md` "Configuration"; `crates/archon-core/src/remote.rs:40-47`) |
| Mount configuration, path normalization, hop-by-hop stripping, lease and Funnel orders | Aether side | `aether: crates/aether-remote/src/mount.rs`; audited on mengsig/aether#414 |
| Idempotency receipts, `resolve_submission` | Aether side | archond keeps `enqueue.idempotency_key` and its tracked requests unchanged; neither is on the device allowlist (`allow.rs:256-298`) |

## Operator standing

Operator authority is a daemon decision backed by an observed message
(`crates/archon-daemon/src/authority/mod.rs:1`). Its channels:

| Path | Who may use it | Evidence |
| --- | --- | --- |
| Operator socket (`archon` CLI) | the Unix user | `operator_only` requests are refused for any agent, whichever client forwards them (`crates/archon-daemon/src/handlers.rs:155-182`); test `agents_are_refused_operator_only_requests_through_any_client` |
| Dedicated Aether chat | the host's own Aether connections only (after H3-A1, H3-A2) | `crates/archon-aether/src/events.rs` `inbox_delivered`; tests `an_aether_paired_device_confers_no_archon_authority`, `a_web_devices_say_carries_no_operator_authority` |
| GitHub operator | a listed login, vouched for only by the watcher's envelope | `crates/archon-aether/src/events.rs:309-339`; payload flags and text markers are refused (unit test in `events.rs`, `only_fresh_verbatim_operator_citations_are_accepted`). GitHub cannot change the watcher (`crates/archon-daemon/src/authority/actions.rs:26-33`; test `github_confirmations_bind_the_thread_and_never_change_the_watcher`) |
| `request_authority` | the current orchestrator, citing a recorded message from the paths above | `crates/archon-daemon/src/authority/mod.rs:21-48`, `evidence.rs:48-73` (60-minute window, verbatim quote). Tests `only_fresh_verbatim_operator_citations_are_accepted`, `stage_callers_and_nested_privileged_requests_are_refused` |

What can never confer it:

- **GitHub text.** An imported issue's body is fenced and marked untrusted
  (`crates/archon-daemon/src/github_sync/import.rs:17-21,105-121`). Only
  Aether's envelope flag makes a GitHub line an operator's
  (`events.rs:309-339`).
- **Relays and stage messages.** `message_orchestrator` and
  `publish_artifact` are the only changes a stage agent may make
  (`handlers.rs:269-279`). Every other change for an agent needs the
  orchestrator's authority (`handlers.rs:207-223`). Messages from `agent`,
  `client` or `system` actors run nothing (test
  `nobody_but_the_operator_upgrades_or_watches_from_the_chat`).
- **Web device lines.** H3-A1.
- **Lessons.** A lesson is a prompt input, never a channel. No code path
  turns one into an `OperatorMessage`: citations are recorded only at
  `crates/archon-daemon/src/operator_commands.rs:50`
  (`grep -rn 'evidence::record' crates` prints that one line).
- **Directives.** `set_directive` is `Change`. It never raises the ceiling
  (`crates/archon-core/src/protocol.rs:506-513`), and only the orchestrator
  may send it for an agent (`handlers.rs:207-223`). It is not on the device
  allowlist.
- **Merge, autonomy, settings, watcher.** All are `OperatorChange` or
  reachable only through `request_authority` / `/archon` commands
  (`protocol.rs:793-830`, `authority/actions.rs:12-49`). None is on the
  device allowlist (`allow.rs` test
  `administration_merges_and_agent_requests_are_refused`).

## The GitHub surface (D1–D3)

| Item | Status | Evidence |
| --- | --- | --- |
| Import treats issue text as data | ✅ | The brief fences title and body in a fence longer than any backtick run and puts the untrusted notice first (`import.rs:102-121`). The title drops control and bidi characters (`import.rs:76-100`). Tests `a_body_cannot_close_the_fence_or_carry_nul`, `a_title_is_one_safe_line_with_the_number` |
| Import is idempotent and limited to registered github.com repositories | ✅ | Key `github:<host>/<repo>#<n>` (`import.rs:31-41`). Only `github.com` issues of a registered origin (`crates/archon-daemon/src/handlers/github_links.rs:111-171`) |
| No injection into commands | ✅ | `gh` runs with an argv, never a shell. Repository and number are validated (`crates/archon-core/src/github.rs:287-315`). Every sync call is asserted read-only `GET` (`fetch.rs:49-66`). ETag hardening: H3-A4 |
| Cached objects are data | ✅ | `github_objects` rows are served only to read replies (`handlers/github_links.rs`) and to the import brief. `grep -rln GithubObject crates/archon-daemon/src` names no prompt or trigger |
| At-most-once posting | ✅ (H3-A3 low) | The intent is recorded before the post, and a pending row is never reposted blindly: the marker is looked up first, and an unknown lookup defers (`crates/archon-daemon/src/github/outbox.rs:1-7,82-112,178-210`). Author-supplied text has markers stripped (`outbox.rs:57-63`, `crates/archon-daemon/src/github/reply.rs:26`). Ship comments: one per (item, run) (`earlier_intent`, `outbox.rs:114-136`) |

## ARCHITECTURE §c as it touches archond

| §c item | Status | Evidence |
| --- | --- | --- |
| Archon `Read` and `Change` within the remote allowlist pass through `request`; `OperatorRead`/`OperatorChange` never do | ✅ Archon side; see H3-X1 | `Request::class()` has no wildcard (`crates/archon-core/src/protocol.rs:749-857`); only `RepoAdd` and `pause`/`resume` depend on fields, and the bridge checks `hard`. The bridge's standing is H3-X1 |
| `shutdown` is never sent | Aether side | `shutdown` is `OperatorChange` and on no allowlist (`allow.rs` test `administration_merges_and_agent_requests_are_refused`) |
| Credentials never cross IPC | ✅ Archon side | No archond reply carries a device secret or its hash (`crates/archon-core/src/remote.rs:167-177`; `api.rs:139-143`) |
| Remote windows load the listener's `https://` origin with the Archon cookie under `/archon`, every Origin, cookie and CSP check unchanged | ✅ Archon side | The checks above are the ones a remote window meets. Cookie `SameSite=Strict`, `HttpOnly`, `Secure` on `https://` (`api.rs:83-85,146-153`) |
| Socket discovery (`$ARCHON_SOCKET`, then the Archon paths) | Aether side | archond's own paths: `crates/archon-core/src/paths.rs` |
| `contextIsolation`, `sandbox`, preload, navigation, `openExternal`, permission handlers, renderer CSP, IPC sender and schema checks, privileged actions and confirmations, managed start, remote-window partitions | Aether side | mengsig/aether#414 |

## Noesis data boundaries (ARCHITECTURE §g)

Noesis lives in Aether and never connects to archond (ARCHITECTURE §g).
Sealed fields are Aether's. Archon serves nothing sealed: no Archon read
reply carries a Noesis field (`grep -rn -i noesis crates/archon-core/src/protocol.rs`
prints nothing). Aether side otherwise.

## Fixes

| Finding | Change | Test (fails before, passes after) |
| --- | --- | --- |
| H3-A1 | `crates/archon-core/src/authority.rs` (`WEB_RELAY_MARK`, `is_web_relay`); `crates/archon-daemon/src/handlers/operator.rs` and `handlers.rs` (a web `say` is sent with the mark); `crates/archon-aether/src/events.rs` (a marked line gets no channel) | `a_web_devices_say_carries_no_operator_authority` (`crates/archon-daemon/tests/it/operator_authority.rs`); `a_web_relay_or_a_paired_device_is_never_an_operator_channel` (`crates/archon-aether/src/events.rs`) |
| H3-A2 | `crates/archon-aether/src/events.rs` (only a host connection's `user` actor is a verified channel) | `an_aether_paired_device_confers_no_archon_authority` (`operator_authority.rs`); `a_web_relay_or_a_paired_device_is_never_an_operator_channel` |

Docs: `docs/remote.md` and `docs/operator-authority.md` now say that a
device's chat line carries no operator authority.

Before the fix: with the two source changes in `events.rs` and
`handlers/operator.rs` taken out (the tests kept), all three tests fail:

```
test events::tests::a_web_relay_or_a_paired_device_is_never_an_operator_channel ... FAILED
test operator_authority::a_web_devices_say_carries_no_operator_authority ... FAILED
test operator_authority::an_aether_paired_device_confers_no_archon_authority ... FAILED
```

With the fix, `cargo test -p archon-aether --lib events` passes 8 of 8, and
`cargo test -p archon-daemon --test it -- operator_authority link_images chat_ops handlers::`
passes 68 of 68.
