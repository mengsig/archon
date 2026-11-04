# Use Archon from your phone

Archon's screens (the queue, each item's stages and verdicts, the operator
chat and the questions waiting on you) are part of the Aether app. Aether's
listener serves them and forwards Archon's requests to archond under the
`/archon` mount, so a browser opens one address for both. One command sets
it up:

```sh
archon remote setup
```

It explains each step as it goes, asks before it publishes anything, and is
safe to run again. This page walks through what it does and what to do when
a step stops. The reference, with the threat model, is
[docs/remote.md](remote.md).

## Before you start

- archon set up (`archon setup`); the wizard runs it for you if it was not.
- Aether 1.3.0 or later: the Aether that Archon uses (your own in shared
  mode, Archon's in dedicated mode) mounts archond. An older one is refused
  with the version it needs; nothing falls back to a separate Archon app.
- [Tailscale](https://tailscale.com/download) on this computer and on your
  phone, both signed in to the same tailnet, with MagicDNS and HTTPS
  certificates turned on in the
  [DNS settings](https://login.tailscale.com/admin/dns).
- No Tailscale Funnel on this computer. Aether never forwards to a mounted
  daemon while its listener's name is open to the internet, so setup
  refuses before it changes anything.

## The short version

```sh
archon remote setup          # on the computer: mounts, publishes, checks
archon daemon restart        # once, when it asks: archond reads the mount at start
archon remote setup          # again: checks the address and shows a QR code
aether remote pair           # pair the same browser with Aether too
```

Scan the QR code with the phone's camera: it opens Aether's Archon pairing
screen, where you type the code it shows. Add the page to the home screen to
open it like an app.

## What the wizard does

1. **Start archond**, if it is not running.
2. **Check Aether and Tailscale.** It asks Aether for its mounts (an Aether
   that cannot mount stops here with the version it needs), checks that
   Tailscale is running, signed in and can serve HTTPS by name, and reads
   `tailscale serve status`. Anything published through Funnel stops it,
   with Aether's own reason, before any listener opens.
3. **Open the listeners.** archond listens on `127.0.0.1:8378`, this
   machine's loopback only, and stays there; Aether's listener opens on its
   own loopback address (`aether remote enable`).
4. **Publish Aether's listener on your tailnet.** It runs
   `tailscale serve --bg --https=443 http://127.0.0.1:8377` after asking you.
   Only devices on your tailnet can reach the address it gets, such as
   `https://box.tail1234.ts.net`; that is the one address, and it is never
   published with Funnel. A port an earlier setup used to publish archond
   directly is withdrawn first (`tailscale serve --https=443 off`, after
   asking). Port 443 taken by something else? It uses 8443 or 10000.
   The address becomes one of Aether's public origins
   (`aether remote enable --origin …`), keeping any it had.
5. **Mount Archon on Aether.** `aether remote mount add /archon --target
   http://127.0.0.1:8378 --kind archon`. Aether checks Funnel itself and
   refuses with its own reason, which setup passes on. Then it writes
   `[remote] mounted_at = "/archon"` and `trusted_origins` (Aether's public
   origins) into Archon's `config.toml`, keeping the rest of the file as
   written, and drops any origin archond used to answer directly.
6. **Restart, once.** archond reads `mounted_at` and `trusted_origins` only
   when it starts, so the first run stops here and asks for
   `archon daemon restart`. Run setup again afterwards.
7. **Check the address.** It fetches `/archon/api/health` through the HTTPS
   address with `curl`, as your phone will. The first request may wait a
   few seconds while Tailscale fetches the certificate.
8. **Pair your browser with Archon.** It opens a one-time code for 5
   minutes and shows a QR code of `https://<address>/#/pair/archon`, the
   Aether app's Archon pairing screen, where you type the code. The code is
   never in the link. The first browser to claim it gets its own Archon
   device; the code then stops working.

Each daemon pairs the browser on its own: `aether remote pair` gives it an
Aether device, and `archon remote setup` or `archon remote pair` an Archon
device. Neither daemon sees the other's credential, and `archon remote
revoke` takes back only Archon's.

## Running it again

Each step reads what is there first: the listeners already open, the
address already published, the origin already Aether's, `/archon` already
mounted on archond, `config.toml` already naming the mount. A run that finds
everything in place says "Nothing changed" and only opens a new pairing
code, so it is also how you pair a second device (`archon remote pair` does
just that step). A run that found archond's listener off and then stopped
switches it back off, so a failed setup leaves remote access as it found it.

## Options

| Flag | Effect |
| --- | --- |
| `--yes` | Publish (and withdraw an old publication of archond) without asking. Needed off a terminal. |
| `--https-port PORT` | The Tailscale HTTPS port for Aether's listener: 443, 8443 or 10000. |
| `--no-pair` | Do everything but open a pairing code. |
| `--json` | Print one JSON document at the end (`remote`, `origin`, `mount`, `changed`, `pairing`) and no steps. |

The installer asks once whether to set remote access up (`ARCHON_REMOTE=1`
answers yes, `ARCHON_REMOTE=0` no; [install.md](install.md)), and the first
run of a bare `archon` offers it right after setting Archon up.

## Visiting archond's listener directly

archond serves no app of its own. A browser that reaches its listener
directly gets a short page that links to Aether's address (or, before
setup, says to run `archon remote setup`); it carries no script.

## What protects you

- Nothing listens until you turn it on, and archond only ever on loopback.
- Only your tailnet can reach the address the wizard publishes, and nothing
  is mounted or published while Funnel publishes anything on this machine.
- Mounted, archond answers only requests that Aether forwards on loopback,
  from browsers on Aether's own origins.
- A device needs the one-time code from your terminal to pair; guesses are
  rate-limited and five wrong ones close the pairing.
- Each device has its own credential per daemon, stored only as a hash.
  `archon remote revoke` takes Archon's back and disconnects it at once.
- A paired browser can do only what Archon's screens offer, and each action
  goes through archond's operator checks, as your CLI's do. Merges,
  promotions, settings and ceilings stay on the CLI.

## Troubleshooting

| What it says | Do |
| --- | --- |
| needs Aether 1.3.0 or later | Upgrade Aether (and restart aetherd), then run it again. |
| Is aetherd running? | Start Aether's daemon, then run it again. |
| … through Tailscale Funnel … Aether refuses | `tailscale funnel reset` (or turn off that port's Funnel), then run it again. |
| Tailscale is not installed | Install it from https://tailscale.com/download. |
| Tailscale is stopped / signed out | `tailscale up`, or sign in at the link it prints. |
| MagicDNS or HTTPS certificates are off | Turn them on at https://login.tailscale.com/admin/dns. |
| this user may not change Tailscale | `sudo tailscale set --operator=$USER` once. |
| Serve is off for your tailnet | Turn it on at the link it prints. |
| Aether already mounts /archon on … | Another target holds the prefix: `aether remote mount remove /archon` if it is no other daemon. |
| Restart it with `archon daemon restart` | Do, then run `archon remote setup` again. |
| cannot edit safely | Add the two lines it prints under `[remote]` in `config.toml` by hand. |
| the listener did not open | Another program holds port 8378: set `[remote] address` in `config.toml` and restart archond. |
| … did not answer | Wait a moment for the certificate and run it again; `aether remote status` shows whether Aether forwards. |
| The browser says "not paired" | The device was revoked or remote access reset: `archon remote pair` and type the new code. |
