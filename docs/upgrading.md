# Upgrading Archon

How to move an installed Archon to a newer release, and what each release
that needs more than `archon update --yes` changes. The update command
itself is in [install.md](install.md#updating), and every release's changes
are in the [changelog](../CHANGELOG.md).

## Contents

- [From 1.4.x to 1.5.0](#from-14x-to-150)
  - [What changes](#what-changes)
  - [Store migrations V19 to V23](#store-migrations-v19-to-v23)
  - [Remote access](#remote-access)
  - [What a paired device may do](#what-a-paired-device-may-do)
  - [Steps, in order](#steps-in-order)
  - [If the update was interrupted](#if-the-update-was-interrupted)
  - [Going back to 1.4.12](#going-back-to-1412)

## From 1.4.x to 1.5.0

### What changes

| | 1.4.12 | 1.5.0 |
| --- | --- | --- |
| Daemon protocol (archond with `archon`, `archon mcp`) | 23 | 24. The Aether app (`archon-web`) may speak 23 or 24; every other client must match exactly. |
| Aether protocol Archon asks for | 24, oldest accepted 17 | unchanged |
| Aether | 1.2.11 | 1.2.11 or later runs the queue. **Aether 1.3.0 or later** is needed for remote access: only it mounts archond at `/archon`. Archon 1.5.0 is released with Aether 1.3.0. |
| Store schema (`archon.db`) | 18 | 23, migrated at archond's first start |
| `remote.json` | format 1 | format 1 while every device is an operator; format 2 once an observer is paired |
| Remote access | archond served its own web app over `tailscale serve` or your proxy | archond is mounted on Aether's listener and its screens are part of the Aether app |
| A hung orchestrator turn | archond nudged it and told you after 30 silent minutes; your messages waited until you pressed Esc | the same notice, after 10 minutes once your message waits, then archond interrupts the turn 5 minutes later, as Esc would, so your messages reach a fresh turn ([troubleshooting](troubleshooting.md#the-orchestrator-or-aether-is-down)) |
| A reviewer plan's effort (`<stage>.review` in `model_plan`) | a floor: archond cancelled a reviewer below it | the reviewer's default, kept where the effort rubric leaves the diff open; otherwise the rubric decides, from `low` for a trivial diff to `x_high` for concurrency or security analysis, and the lead states the reason in one line, lower or higher, up to `[models] max_effort`; a reviewer on a model the plan does not name is still cancelled |
| `config.toml` | | new keys `[remote] mounted_at`, `[remote] trusted_origins` and `[learning] holdout_rate` |

A 1.5.0 `archon` and a 1.4.x archond refuse each other at `hello`, so
upgrade both together: `archon update --yes` restarts archond for you. A
stage agent's `archon mcp` that outlives the update restarts itself on the
new binary once archond refuses it, as since 1.4.2, so running stages carry
on.

### Store migrations V19 to V23

archond 1.5.0 upgrades a schema 18 store in place the first time it starts,
in one write transaction: either every step lands or none does.

| Version | Adds |
| --- | --- |
| V19 | `changes`, the gapless change stream, seeded with one change per item at its current state, and the item transitions behind `item_history`, which start empty. |
| V20 | `github_links`, backfilled from item origins (`origin` links) and recorded pull requests (`delivered` links). An origin number too large to store is skipped. |
| V21 | `github_objects`, the read-only cache of linked issues and pull requests. |
| V22 | `github_outbox`, which posts each GitHub comment at most once. |
| V23 | `lessons.cluster` and `lesson_trials`. No lesson row is rewritten. |

The migrations are forward-only. Archon 1.4.12 refuses a migrated store
("store schema version 23 is newer than this archon supports (18); upgrade
archon"), and archond takes **no copy of its own** before it migrates: back
the store up yourself first (step 2 below).

### Remote access

- `archon remote setup` now mounts archond on an Aether listener at
  `/archon` instead of publishing archond. It needs Aether 1.3.0 or later
  (your own Aether in shared mode, Archon's in dedicated mode), refuses while
  anything on the machine is published through Tailscale Funnel, withdraws
  a `tailscale serve` that published archond directly, publishes Aether's
  listener as the one origin, and asks for one `archon daemon restart`
  ([remote-setup.md](remote-setup.md)).
- `--proxy-url` and `--local` are removed. A proxy that pointed at archond
  finds no app there any more: a direct visit gets a short page, without
  scripts, that links to Aether's address.
- Setup writes two keys into `config.toml`, keeping the rest of the file:

  ```toml
  [remote]
  mounted_at = "/archon"                             # served behind Aether's mount
  trusted_origins = ["https://box.tail1234.ts.net"]  # Aether's public origins, at most 16
  ```

  archond reads them only when it starts. With them it scopes its device
  cookie to `/archon`, answers only loopback `Host` values on its own port,
  and accepts only these origins.
- Devices paired with 1.4.x stay valid, as operators, until
  `archon remote revoke` removes them. A browser that paired on the hostname
  Aether now publishes still sends its old cookie (scoped to `/`), and the
  mount accepts it; one that paired on another hostname must pair again with
  `archon remote pair`. Aether's own device is paired separately, with
  `archon aether remote pair`. Pairing again does not revoke the old device:
  revoke it to retire that access.

### What a paired device may do

- A device is an `operator` (the default, and every device paired before
  1.5.0) or an `observer`, paired with `archon remote pair --observer`,
  which only reads.
- A device's requests run with the Aether app's standing (`archon-web`),
  not the operator CLI's. `pause` and `resume` with `hard` are refused to
  every device, and a soft resume no longer lifts the operator's hard pause.
- From the security audit ([vnext-audit.md](security/vnext-audit.md)),
  a device never carries operator authority:
  - a web device's `say` reaches the orchestrator marked, so `/archon`,
    `/watch`, `/merge` and `/upgrade` in it are not run and the
    orchestrator cannot cite it for `request_authority`;
  - chat from a device paired with Aether, rather than the host's own Aether
    connection, is not operator chat either;
  - a web `answer` cannot confirm an authority change, and its reply to an
    Aether question goes out marked like `say`;
  - an answer from an Aether-paired device in the notification center is
    refused for Archon's gates and questions.

  Answer these on the host instead: `archon answer`, the TUI, or Aether on
  this machine.
- Devices may also read the change stream, item history, the task graph,
  run plans and GitHub links, and operators may link and unlink issues
  and import one ([remote.md](remote.md#what-a-device-may-do)).

### Steps, in order

1. **Check Aether.** `aether --version`. To use remote access, upgrade
   Aether to 1.3.0 or later first, as its own upgrade notes say. The queue
   runs on Aether 1.2.11 as before.
2. **Back up.** Stop archond and copy the store and the files beside it:

   ```sh
   archon daemon stop
   cd ~/.local/share/archon      # or $ARCHON_DATA_DIR; ~/Library/Application Support/archon on macOS
   cp archon.db archon-1.4.db
   if [ -f archon.db-wal ]; then cp archon.db-wal archon-1.4.db-wal; fi
   if [ -f remote.json ]; then cp remote.json remote-1.4.json; fi
   ```

   A cleanly stopped archond leaves no `archon.db-wal`, and `remote.json`
   is absent if remote access was never configured; going back relies on
   knowing which of the two you copied. Keep a copy of `config.toml`
   (`~/.config/archon`, or `$ARCHON_CONFIG_DIR`) as well.
3. **Install 1.5.0.** `archon update --version v1.5.0 --yes` (or `archon
   update --yes` once it is the latest release).
4. **Start and check.** `archon daemon start` migrates the store. Then
   `archon --version` says `archon 1.5.0`, and `archon status` and `archon
   doctor` report the daemon healthy. `archon orchestrator restart` gives a
   running orchestrator the new tools if the update did not.
5. **Remote access, if you used it.** Run `archon remote setup`, then
   `archon daemon restart` when it asks, then `archon remote setup` again.
   A browser paired on the hostname Aether now publishes keeps working; pair
   one that paired on another hostname with the code setup shows (or
   `archon remote pair`), and with Aether through `archon aether remote
   pair` if it has no Aether device yet. Revoke the 1.4.x devices you no
   longer need with `archon remote revoke`.

### If the update was interrupted

A plain `archon update --yes` from 1.4.x installs 1.5.0 and then asks the
running archond to restart. Stopped in between (Ctrl+C, a closed terminal),
it leaves the 1.4.x archond running, and the installed 1.5.0 cannot reach
it: commands say "this archon speaks daemon protocol 24 but archond does
not", and `archon daemon stop` and `archon daemon restart` fail the same
way. Stop it by the pid it writes beside its socket, wait until it has
exited (a few seconds), then start 1.5.0's; running stage agents keep
running in Aether:

```sh
pid=$(cat "$XDG_RUNTIME_DIR/archon/archond.pid")
kill "$pid"
while kill -0 "$pid" 2>/dev/null; do sleep 1; done
archon daemon start
```

Without `$XDG_RUNTIME_DIR`, as on macOS, the socket and its `archond.pid`
are in the data directory; with `$ARCHON_SOCKET` set, the pid file is that
path ending in `.pid`.

### Going back to 1.4.12

Stop archond, install 1.4.12 and put the copies back. Everything 1.5.0
recorded since the copy is lost.

```sh
archon daemon stop
archon update --version v1.4.12
cd ~/.local/share/archon
rm -f archon.db-wal archon.db-shm
cp archon-1.4.db archon.db
if [ -f archon-1.4.db-wal ]; then cp archon-1.4.db-wal archon.db-wal; fi
if [ -f remote-1.4.json ]; then cp remote-1.4.json remote.json
elif [ -f remote.json ]; then mv remote.json remote-1.5.json; fi
```

A `remote.json` that 1.5.0 wrote where 1.4.12 had none is moved aside: its
devices were paired from the Aether app, and 1.4.12 refuses the format 2 an
observer brings. 1.4.12 also refuses `config.toml` keys it does
not know: remove `mounted_at` and `trusted_origins` under `[remote]` and
`holdout_rate` under `[learning]` (or restore your copy), then `archon daemon
start`. Aether's `/archon` mount points at archond's listener, which 1.4.12
serves as its own app: remove it with `archon aether remote mount remove
/archon`, then run 1.4.12's `archon remote setup` to publish archond again.
