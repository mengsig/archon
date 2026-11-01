# Development and benchmarks

See [CONTRIBUTING.md](../CONTRIBUTING.md) for contribution requirements.

## Overview graphic

Edit [`assets/src/archon_graphic.src.svg`](assets/src/archon_graphic.src.svg),
the hand-written vector source. From the repository root, rebuild with
Node.js, npm and curl installed:

```sh
cd docs/assets/src
npm ci
./fetch-fonts.sh
npm run build
cd ../../..
```

The build outlines all text using Fira Sans, Fira Mono and Caveat, keeping
[`assets/archon_graphic.svg`](assets/archon_graphic.svg) self-contained for
GitHub. Commit both SVGs; downloaded fonts and `node_modules` are ignored.
To preview with `rsvg-convert` (from librsvg) installed:

```sh
rsvg-convert -w 1600 docs/assets/archon_graphic.svg -o docs/assets/archon_graphic.png
```

The PNG is ignored. Inspect it for overlaps, clipping and intact mascots
before committing.

## Benchmarks

`bench/` runs the same hidden-graded tasks through four arms: Archon, plain
Aether, `claude -p` and `codex exec`. Agents never see the graders, and each
trial runs in its own isolated workspace outside the checkout.

```sh
cargo run -p archon-bench -- run --dry-run
cargo run -p archon-bench -- run --arms archon,aether,claude --trials 3 --max-usd 20
```

### Results

v1 run, 2026-09-26: 8 tasks, effort `high`, Opus 5.5 for `archon`, `aether`
and `claude`, and codex's default model (gpt-5.6-sol). Values are the mean over
graded trials; the budget guard stopped the run at n=1–2 per cell.

| Arm | Success | Wall s | Cost $ | Operator msgs |
|---|---:|---:|---:|---:|
| archon | 12/12 | 279 | 2.02 | 3.7 |
| aether | 13/13 | 91 | 0.42 | 3.5 |
| claude | 12/12 | 77 | 0.38 | 1.0 |
| codex | 13/13 | 215 | 0.38 (est.) | 1.0 |

Every arm solved every graded trial. Archon costs about 5× a single agent
because each item goes through its full review pipeline. The method, per-task
results and caveats are in [`bench/README.md`](../bench/README.md).

## Layout

| Path | What |
| --- | --- |
| `crates/archon-core` | Shared domain types, config, model policy, daemon protocol |
| `crates/archon-aether` | Rust client for Aether's socket protocol |
| `crates/archon-queue` | SQLite backlog and dispatch planner |
| `crates/archon-pipeline` | Pipeline loader, templating, run state machine |
| `crates/archon-client` | Client for the Archon daemon socket |
| `crates/archon-mcp` | MCP server exposing Archon to the orchestrator |
| `crates/archon-lavish` | Optional lavish-axi review-surface plugin |
| `crates/archon-daemon` | `archond`, the Archon daemon |
| `crates/archon` | `archon` CLI and quiet operator chat |
| `pipelines/` | Built-in pipelines |
| `bench/` | Benchmarks |

The design is in [`docs/ARCHITECTURE.md`](ARCHITECTURE.md), including
the Aether limitations Archon works around (§17).

## Development

For a local source installation:

```sh
cargo install --locked --path crates/archon
archon setup
```

```sh
make ci      # fmt check, clippy -D warnings, rustdoc -D warnings, tests
```

`make ci` holds one of `CI_HOST_SLOTS` (default 2) host-wide CI slots while
it runs, so at most that many full runs, Aether's included, load the host at
once; see [CONTRIBUTING.md](../CONTRIBUTING.md).

`make lint`, and so `make ci`, also checks `install.sh` and the scripts in
`scripts/`, and needs [ShellCheck](https://www.shellcheck.net/) installed. The installer's tests
run it under `dash` when there is one, else under `sh`; `ARCHON_TEST_SH`
names another shell.

## The mobile web app

The app lives in `crates/archon-remote/web` as plain ES modules and CSS, with
no build step; `build.rs` embeds the shipped files into archond. Its tests:

```sh
make e2e     # unit tests, then every flow in headless Chromium against archond
```

`make e2e` builds `archon`, installs the npm packages and Chromium on first
use, starts archond in a temp home under `/tmp`, sets remote access up with
`archon remote setup --local` and pairs with `archon remote pair`. It skips
with a message when Chromium cannot be installed, unless `CI` is set. The
release pull request into `main` runs it in CI. `node scripts/screens.js`
in that directory regenerates the screenshots in `docs/assets/remote/`;
`crates/archon-remote/web/README.md` has the rest.

## Releasing

Pushing a tag `v<version>` that equals the workspace version in `Cargo.toml`
runs `.github/workflows/release.yml`. It builds every target, installs each
archive with `install.sh` and runs it, then opens a **draft** release with
the archives, their checksums, `SHA256SUMS` and `VERSION`; publishing the
draft makes it the release the installer and `archon update` download.

`archon release check [owner/repo]` audits release health (default:
`mengsig/archon`). It checks all version tags, the four platform archives and
their `.sha256` files, `SHA256SUMS`, `VERSION`, the highest stable SemVer as
Latest, and whether the latest completed `ci.yml` run assigned a job to a runner.
Queued or waiting runs are not treated as failures to start.
Pre-releases are checked for assets but do not compete for Latest. This uses
the configured `github.binary` and its GitHub authentication; `--json` emits
`repo`, `ok` and `problems`. An unhealthy release or an API failure exits 1.
Doctor runs this for Archon and Aether, warning if GitHub is unavailable.

Before creating the draft, the workflow runs the same check using the newly
built binary with `--pending-tag "$GITHUB_REF_NAME" --assets dist`. Only that
tag is exempt from already having a published release; its local assets
must exist, be nonempty, and have a matching `VERSION`. Other historical
release problems block the gate until the operator repairs them. The
existing checksum job verifies archive digests. The gate precedes draft
creation; publishing remains an operator action in GitHub.

Write the version's section in `CHANGELOG.md` before you tag: the text of the
draft is that section followed by the install command, and the workflow
fails in its first job when there is no `## <version>` section. To read the
text beforehand, run `sh scripts/release-notes.sh <version>`. Each archive
holds `archon`, `LICENSE` and `THIRD-PARTY-NOTICES`, packed by
`scripts/package-release.sh`; update `THIRD-PARTY-NOTICES` when a dependency
with a licence it does not name is added.

To try the build without a release:

```sh
gh workflow run release.yml -f dry_run=true
```

When Actions cannot run, build from an existing tag locally with Rust/rustup,
`cargo-zigbuild` (`cargo install cargo-zigbuild --locked`), Zig, `file`, GNU
`sha256sum` and `tar` on PATH. Install all four workflow targets with
`rustup target add`; Darwin cross builds may also need an Apple SDK via
`SDKROOT`. Missing compilers, libraries or SDKs fail the build; no substitute
assets are produced.

```sh
CARGO_TARGET_DIR="$HOME/.cache/cargo-target/archon-release" \
  sh scripts/build-release-local.sh v1.2.5 "$HOME/.cache/archon-release-assets/v1.2.5"
gh release upload v1.2.5 "$HOME/.cache/archon-release-assets/v1.2.5/"*
```

The output directory must be empty. The script uses a temporary detached
worktree under `${XDG_CACHE_HOME:-$HOME/.cache}`, removes it on exit, and
keeps a supplied `CARGO_TARGET_DIR` for reuse. It packages each successful
target but writes `SHA256SUMS` and `VERSION` only after all four succeed
and their checksums verify. After a failure, keep any needed diagnostics
and choose a new empty output directory for a retry. Before uploading,
run the workflow's isolated installer smoke test against a local server
and verify the four-line `SHA256SUMS` with `sha256sum --check --strict`.
Publish or mark the release Latest only once its complete assets are uploaded.

See [CONTRIBUTING.md](../CONTRIBUTING.md) for live tests, the benchmark and the
commit conventions.
