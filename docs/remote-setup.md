# Use Archon from your phone

Archon's mobile web app shows the queue, each item's stages and verdicts,
the operator chat and the questions waiting on you, and lets you enqueue,
cancel, retry, pause and resume, from your phone's browser. One command
sets it up:

```sh
archon remote setup
```

It explains each step as it goes, asks before it publishes anything, and is
safe to run again. This page walks through what it does and what to do when
a step stops. The reference, with the threat model, is
[docs/remote.md](remote.md).

## Before you start

- archon set up (`archon setup`); the wizard runs it for you if it was not.
- [Tailscale](https://tailscale.com/download) on this computer and on your
  phone, both signed in to the same tailnet, with MagicDNS and HTTPS
  certificates turned on in the
  [DNS settings](https://login.tailscale.com/admin/dns). Or an HTTPS reverse
  proxy of your own ([below](#using-your-own-proxy)).

## The short version

```sh
archon remote setup          # on the computer: publishes and shows a QR code
```

Scan the QR code with the phone's camera, give the phone a name, and you
are in. Add the page to the home screen to open it like an app.

## What the wizard does

1. **Start archond**, if it is not running.
2. **Open the listener.** archond serves the app on `127.0.0.1:8378`, this
   machine's loopback only; nothing off the machine can connect yet. It
   stays on across archond restarts until `archon remote disable`.
3. **Publish on your tailnet.** It checks that Tailscale is running,
   signed in, and can serve HTTPS by name, then runs
   `tailscale serve --bg --https=443 http://127.0.0.1:8378` after asking
   you. Only devices on your tailnet can reach the address it gets, such as
   `https://box.tail1234.ts.net`; it is never published with Funnel. Port
   443 taken by something else? It uses 8443 or 10000 instead. Already
   published by an earlier run? It reuses that.
4. **Check the address.** It fetches `/api/health` through the HTTPS
   address with `curl`, as your phone will. The first request may wait a
   few seconds while Tailscale fetches the certificate.
5. **Pair your phone.** It opens a one-time code for 5 minutes and shows it
   with a QR code. The first device to claim it is paired and gets its own
   credential; the code then stops working.

## Running it again

Each step finds what an earlier run did: the listener already open, the
address already published. Running it again only opens a new pairing code,
so it is also how you pair a second device (`archon remote pair` does just
that step).

## Options

| Flag | Effect |
| --- | --- |
| `--yes` | Publish without asking. Needed off a terminal. |
| `--proxy-url URL` | Use your own HTTPS proxy at this origin instead of Tailscale. |
| `--local` | Publish nothing; the app answers on this computer only. |
| `--https-port PORT` | The Tailscale HTTPS port: 443, 8443 or 10000. |
| `--no-pair` | Do everything but open a pairing code. |
| `--json` | Print one JSON document at the end (`remote`, `origin`, `pairing`) and no steps. |

The installer asks once whether to set the app up (`ARCHON_REMOTE=1`
answers yes, `ARCHON_REMOTE=0` no; [install.md](install.md)), and the first
run of a bare `archon` offers it right after setting Archon up.

## Using your own proxy

Point a reverse proxy that terminates TLS at `http://127.0.0.1:8378`,
forwarding WebSocket upgrades and keeping the `Host` header the browser
sent, then:

```sh
archon remote setup --proxy-url https://archon.example.com
```

archond answers only the origins it was told about, so the proxy must not
rewrite `Host` or `Origin`. Off a tailnet, anyone who reaches the proxy can
reach the pairing page: put your own authentication in front of it.

## What protects you

- Nothing listens until you turn it on, and then only on loopback.
- Only your tailnet can reach the address the wizard publishes.
- A device needs the one-time code from your terminal to pair; guesses are
  rate-limited and five wrong ones close the pairing.
- Each device has its own credential, stored on archond only as a hash.
  `archon remote revoke` takes it back and disconnects it at once.
- A paired phone can do only what the app offers, and each action goes
  through archond's operator checks, as your CLI's do. Merges, promotions,
  settings and ceilings stay on the CLI.

## The screens

| Screen | What it shows |
| --- | --- |
| Queue | Items with what each waits on; filter by state, held-up items, repository, pipeline and priority, or search. |
| Item | Stages, verdicts, summaries, cost, pull request, artifacts and reports; cancel or retry. |
| Chat | The operator chat with the orchestrator, live. |
| Inbox | Questions, including stage gates, and notifications waiting on you, answered in place. |
| New item | Enqueue with repository, title, brief, pipeline and review level. |
| Overview | Agents against caps, budgets and spend, the cost report, repositories and pipelines; pause and resume. |
| Settings | Theme, this device and the versions; forget this device. |

Pictures of each, light and dark, are in [remote.md](remote.md#screens).

## Troubleshooting

| What it says | Do |
| --- | --- |
| Tailscale is not installed | Install it from https://tailscale.com/download, or use `--proxy-url` or `--local`. |
| Tailscale is stopped / signed out | `tailscale up`, or sign in at the link it prints. |
| MagicDNS or HTTPS certificates are off | Turn them on at https://login.tailscale.com/admin/dns. |
| this user may not change Tailscale | `sudo tailscale set --operator=$USER` once. |
| Serve is off for your tailnet | Turn it on at the link it prints. |
| publishes archond … through Tailscale Funnel | `tailscale funnel --https=PORT off`; the wizard never reuses a public port. |
| the listener did not open | Another program holds port 8378: set `[remote] address` in `config.toml` and restart archond. |
| … did not answer | Wait a moment for the certificate and run it again; check that the proxy keeps `Host`. |
| The phone says "not paired" | The device was revoked or remote access reset: `archon remote pair` and scan again. |
