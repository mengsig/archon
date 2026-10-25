# Installing Archon

Archon is one program, `archon`. Releases are built for:

| System | CPU | Release target |
| --- | --- | --- |
| Linux | x86_64 | `x86_64-unknown-linux-musl` |
| Linux | arm64 | `aarch64-unknown-linux-musl` |
| macOS 11 or newer | Apple Silicon | `aarch64-apple-darwin` |
| macOS 11 or newer | Intel | `x86_64-apple-darwin` |

The Linux binaries are statically linked and need no particular libc. There
is no native Windows build; on Windows use WSL, where the Linux build runs.
On any other system, [build from source](#building-from-source).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/mengsig/archon/main/install.sh | sh
```

The installer needs no `sudo`. It:

1. picks the release archive for your system and downloads it with `curl`,
   or `wget` when there is no `curl`;
2. checks the archive against its published SHA-256 and installs nothing
   when they differ, or when neither `sha256sum` nor `shasum` exists;
3. puts `archon` in `~/.local/bin`, replacing an older one in a single
   rename;
4. looks for [Aether](#aether) and offers to install it when it is missing;
5. prints what it installed, [other programs](#other-programs-archon-uses)
   that are missing, whether the directory is on your `PATH` (and the line
   to add when it is not), and the next command, `archon setup`.

It writes only to the install directory. It does not edit your shell
profile, and it never touches Archon's configuration or data.

### Options

Set these in the environment of `sh`, after the pipe:

```sh
curl -fsSL https://raw.githubusercontent.com/mengsig/archon/main/install.sh | ARCHON_VERSION=v1.4.6 sh
```

| Variable | Effect |
| --- | --- |
| `ARCHON_VERSION` | Install this release, such as `v1.4.6`, instead of the latest. |
| `ARCHON_INSTALL_DIR` | Install here instead of `~/.local/bin`. The directory must be yours to write. |
| `ARCHON_SKIP_AETHER=1` | Do not install Aether; print the command that does. |
| `ARCHON_NONINTERACTIVE=1` | Ask nothing: a missing Aether is installed without a question. |
| `ARCHON_INSTALL_BASE_URL` | Download from this address instead of `https://github.com/mengsig/archon/releases`. It must serve `latest/download/<file>` and `download/<tag>/<file>`. `archon update` honours it too. |
| `ARCHON_AETHER_INSTALL_URL` | Fetch Aether's installer from this address. |
| `https_proxy`, `http_proxy` | Honoured by `curl` and `wget`. |

The installer exits with 0 when everything is in place, 1 when it installed
nothing, and 2 when Archon is installed but installing Aether failed; it
then prints the command to retry that step.

When there is no terminal to ask on, as in a script or a container build, a
missing Aether is installed without a question, and the output says so.

## Aether

Archon runs its agents on [Aether](https://github.com/mengsig/aether), a
separate program, and does not work without it. When the installer finds no
`aether` on your `PATH` or in the install directory, it runs Aether's own
installer with the same install directory.

What Archon needs is a protocol, not a version number: an `archon` speaks a
range of Aether protocol versions, and an `aether` serves a range. They work
together when the ranges overlap. The installer and `archon update` print
both ranges and warn when they do not overlap; `archon doctor` checks the
running daemon. To see them yourself:

```sh
archon update --requirements    # the protocols this archon speaks
aether schema | grep -E '^  "(min_)?protocol_version"'
```

Update an Aether that is too old with `aether update`.

Archon 1.4.6 needs **Aether 1.2.5**: it is released with and tested on that
release, which the installer and `archon update` install. Archon checks
Aether's protocol (24), not its version, so it does not refuse an older
aetherd that serves protocol 24, but only Aether 1.2.5 is supported with it.

## Other programs Archon uses

The installer names each of these that is missing; it installs none of
them. `archon doctor` checks the ones your configuration needs.
If you are already signed in to Claude Code or Codex, neither Archon nor Aether needs a separate login for that engine.

| Program | Used for | Required? | Install |
| --- | --- | --- | --- |
| [Aether](https://github.com/mengsig/aether) (`aether`) | Runs the orchestrator and stage agents | Yes | The Archon installer offers it; see [Aether](#aether). |
| [git](https://git-scm.com/install/) 2.38 or later | A worktree and a branch for every item | Yes | `brew install git`, or your distribution's package |
| [Claude Code](https://code.claude.com/docs/en/quickstart) (`claude`) | Claude engine and its login | One engine required | `curl -fsSL https://claude.ai/install.sh \| bash`, then run `claude` once to log in |
| [Codex](https://github.com/openai/codex) (`codex`) | Codex engine and its login | Alternative engine | `npm install -g @openai/codex`, then `codex login` |
| [GitHub CLI](https://cli.github.com/) (`gh`) | The `pr` ship mode and the GitHub watcher | Optional; local work works without it | `brew install gh`, then `gh auth login` |
| [Node.js / npm](https://nodejs.org/en/download) | Runs Lavish; installs npm CLIs | Optional with Claude Code and no Lavish | `brew install node` (Node 22 or newer for Lavish) |
| [lavish-axi](https://github.com/kunchenguid/lavish-axi) | Annotated HTML review pages | Optional | `npm install -g lavish-axi` |
| [Rust / Cargo](https://rustup.rs/) | Builds Archon from source | Only for source builds | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| [Playwright](https://playwright.dev/docs/intro) | Browser journeys in the web testing pipelines | Optional | `npm init playwright@latest` in the tested project |

## Verifying a download by hand

Every release carries, for each archive, a `.sha256` file, and one
`SHA256SUMS` file that lists all of them.

```sh
tag=v1.4.6
target=x86_64-unknown-linux-musl
base=https://github.com/mengsig/archon/releases/download/$tag
curl -fsSLO "$base/archon-$target.tar.gz"
curl -fsSLO "$base/SHA256SUMS"
sha256sum --check --ignore-missing SHA256SUMS    # macOS: shasum -a 256 --check --ignore-missing SHA256SUMS
tar -xzf "archon-$target.tar.gz" archon
mkdir -p ~/.local/bin && mv archon ~/.local/bin/archon
```

The check must print `archon-<target>.tar.gz: OK`. The checksum shows that
the archive is the one the release published; it comes from the same place
as the archive, so it does not prove who published it. Beside `archon`, the
archive holds `LICENSE` and `THIRD-PARTY-NOTICES`; the installer and
`archon update` install only `archon`.

## Updating

```sh
archon update            # install the latest release when it is newer
archon update --check    # only report: exit 0 when up to date, 10 when a newer release exists
archon update --version v1.4.6    # install exactly this release
```

`archon update` downloads the same archive as the installer, makes the same
SHA-256 check, and renames the new binary over the old one in the same
directory. A failed download or check leaves the installed `archon` as it
was, and so does an update you interrupt, which also removes what it had
downloaded. The rename is the last thing an update does, after every
download and check: a signal that arrives later than that stops nothing,
and the update reports the new version. An update that was killed outright
can leave a `.archon-update.*` directory beside the binary; a later update
removes it once it is more than an hour old. It uses the network only when
you run it,
and sends nothing but the download requests. Like the installer, it runs `curl` or `wget`,
`sha256sum` or `shasum`, and `tar`.

The install itself interrupts no running item. A running `archond` keeps
the old version until it restarts. On a terminal `archon update` then says
what it would restart and asks `[Y/n]`; `--yes` answers for scripts, and
`--no-restart` installs `archon` alone:

```sh
archon update --yes                  # install, update Aether, restart everything
archon update --yes --wait 600 --requeue
```

An accepted restart does, in order: `aether update`; the drained restart
of `archond` and, in dedicated mode, Archon's own aetherd (`archon daemon
restart`, with the same `--wait` and `--requeue`: running items resume as
after any daemon restart); and `archon orchestrator restart`, which gives
the orchestrator the new tools. In shared mode your own aetherd is never
restarted; Archon's tools in it are refreshed with `aether mcp refresh
archon`. Nothing is restarted when `archond` is not running or already runs
the installed version, and a failed `aether update` is reported as a
warning (exit 1) after the restart. Without `--yes`, off a terminal, the
command prints what to run by hand:

```sh
archon daemon stop --keep-aether
archon aether mcp refresh archon
archon orchestrator restart
```

It also reports whether your Aether serves what the new version needs, and
prints `aether update` when it does not.

### The check at launch

Each `archon`, `archon tui` and `archon chat` on a terminal looks for a
newer release of Archon and of Aether before it opens, and offers:

```text
Archon 1.4.7 is available (this is 1.4.6).
Update and restart now? [Y/n/s] (s: skip these versions)
```

`Y` (or Enter) runs `archon update --yes` and then starts the new `archon`;
`n` goes on and asks again at the next launch; `s` skips these versions
until a newer one is released. The check waits at most 1.5 seconds for
GitHub, reuses an answer for `[update] cache_for` (an hour), is silent
offline and retries ten minutes after a failure, and sends only the
requests `archon update --check` and `aether update --check` send. The
answer and the skipped versions are kept in `update-check.json` in the data
directory. Switch it off with `[update] check_on_launch = false`, or for one
shell with `ARCHON_NO_UPDATE_CHECK=1`. Nothing is asked off a terminal.

`archon status`, `archon doctor` and the orchestrator's `queue_status` say
when the running orchestrator is behind: for example `generation 7, created
by 1.2.3; prompt outdated; 3 new lessons`. A restart catches it up.

Or let archond do all of it without interrupting work:

```sh
archon daemon upgrade --archon latest              # --aether too, in dedicated mode
archon daemon upgrade --archon main --wait 600 --requeue
archon daemon restart                              # the same drained restart, installing nothing
```

archond holds dispatch until running stage agents finish or `--wait`
seconds pass, then installs (`<program> update` for an installer's binary,
`cargo install --locked --git` for one in cargo's bin directory) and
restarts itself and Archon's own aetherd through a detached helper and
reports the old and new versions in `archon log`. When the wait runs out it
gives the upgrade up without installing anything, unless `--requeue` says to restart anyway and retry
the stages it stopped. The orchestrator's chat takes the same as `/upgrade`
([guide](guide.md#upgrading-and-watching-from-the-orchestrators-chat)).

Installation keeps dispatch held until the restart, so a source build can
extend the pause beyond `--wait`. Between installation and restart, a new
CLI may be unable to talk to the old archond after a protocol change;
wait for the detached restart to finish. In shared mode, or when keeping
Aether running, existing agents retain their old `archon mcp` processes.
After an Archon protocol change those tools may be incompatible with the
new archond; restart the affected agents (`archon orchestrator restart`
for the orchestrator). Dedicated mode restarts Aether and its tools too.

### Back up before a release that migrates the store

A release may migrate the queue database (`archon.db` in the data
directory) when its `archond` first starts, and an older `archond` refuses
a migrated store: "store schema version 10 is newer than this archon
supports (7)". The migration is one-way. 1.1.0 migrates a 1.0 store, so
back it up first:

```sh
archon daemon stop
cd ~/.local/share/archon      # or $ARCHON_DATA_DIR; ~/Library/Application Support/archon on macOS
cp archon.db archon-1.0.db
archon update
```

An `archond` that stopped cleanly leaves no `archon.db-wal` beside the
store. If there is one, copy it too, as `archon-1.0.db-wal`.

To go back, stop the new `archond`, install the old release and put the
copy back. Everything the newer release recorded since the copy is lost.

```sh
archon daemon stop
archon update --version v1.0.1
cd ~/.local/share/archon
rm -f archon.db-wal archon.db-shm
cp archon-1.0.db archon.db    # and archon-1.0.db-wal as archon.db-wal, if you copied one
archon daemon start
```

`archon update --version` with an older release installs it even when
the store was migrated; the old `archond` then refuses to start until the
copy is back.

`archon update` only replaces an `archon` that the installer put in a
directory you can write. For a `cargo install` or a build directory it
refuses and prints the command to use instead. Running the installer again
also updates.

## Uninstalling

```sh
archon daemon stop             # stop archond and Archon's own aetherd
rm ~/.local/bin/archon         # or the directory you chose with ARCHON_INSTALL_DIR
```

Remove `~/.local/bin/aether` too if you no longer use Aether; its own data
is described in Aether's documentation.

If you ran Archon in shared mode (`[aether] mode = "shared"`), `archon setup`
appended an `archon` MCP server to your own Aether configuration, and no
command removes it. Open `~/.config/aether/config.toml` (macOS:
`~/Library/Application Support/aether/config.toml`; `AETHER_CONFIG_DIR`
moves it) and delete this block, from the comment to the `classes` line:

```toml
# Added by `archon setup`. Remove this block to unregister Archon.
[[mcp.servers]]
name = "archon"
command = "/home/you/.local/bin/archon"
args = ["mcp"]
send_caller_meta = true
classes = { ... }
```

Then restart your `aetherd` so that it stops offering Archon's tools. In
the default, dedicated mode there is nothing to remove from your Aether
configuration: Archon's registration is in its own Aether instance, in the
directories below.

Uninstalling the program leaves your data in place. To remove that as well:

| What | Linux | macOS |
| --- | --- | --- |
| Configuration: `config.toml`, your own pipelines, Archon's Aether instance | `~/.config/archon` | `~/Library/Application Support/archon` |
| Data: the queue database, worktrees (by default), logs, reports, the Aether instance's data | `~/.local/share/archon` | `~/Library/Application Support/archon` |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` move the Linux directories, and
`ARCHON_CONFIG_DIR` and `ARCHON_DATA_DIR` move them on either system. For a
second, private setup set both, and `ARCHON_SOCKET` too: where only some of
the three are set, no command starts archond for you
([why](guide.md#which-commands-start-archond)). A
repository's own settings are in its `.archon/` directory. Worktrees hold
unmerged work: look before you delete the data directory.

## Building from source

You need git and [rustup](https://rustup.rs/); `rust-toolchain.toml` pins
the Rust version, and rustup installs it on first use.

```sh
git clone https://github.com/mengsig/archon
cd archon
cargo install --locked --path crates/archon
archon setup
```

This installs `archon` to `~/.cargo/bin`; `make install` runs the same
`cargo install` command. Update a source build with `git pull` and the same
command; `archon update` does not replace it.
