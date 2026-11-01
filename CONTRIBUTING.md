# Contributing to Archon

Thank you for helping. Bug reports, fixes, documentation and new pipelines
are all welcome.

## Before you start

- **Small changes** (a bug fix, a typo, a clearer error): open a pull request.
- **Large changes** (a new command, a protocol or schema change, a new
  dependency, a change in behaviour users rely on): open an issue first and
  agree on the direction, so your work is not wasted.
- **Security problems**: do not open an issue. See [SECURITY.md](SECURITY.md).

Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Build and test

You need `git`, `make` and [rustup](https://rustup.rs/). `rust-toolchain.toml`
pins the toolchain, and rustup installs it on first use. The benchmark's
tests also run its Python and Node fixtures, so `python3` and `node` must be
on `PATH`. The tests need no Aether daemon, no model and no account.

```sh
git clone https://github.com/mengsig/archon
cd archon
make build   # cargo build --workspace
make fmt     # cargo fmt --all
make ci      # fmt check, clippy -D warnings, rustdoc -D warnings, all tests
```

`make ci` is what GitHub CI checks on every pull request into `main` that
changes code, and it must pass before a change merges. The Makefile caps parallel jobs with
`CARGO_BUILD_JOBS=4`; override it on the command line if you have cores to
spare (`make ci CARGO_BUILD_JOBS=16`). CI runs the tests through
[cargo-nextest](https://nexte.st/) (`make test-nextest`) because it is faster;
it is optional locally.

A passing `make ci` holds for any later commit with the same tree
(`git rev-parse HEAD^{tree}`). After it passed, a change touching only
Markdown, `CHANGELOG.md` or `docs/` needs only the link check
(`cargo test -p archon --test docs_links`) and the notice check
(`cargo test -p archon --test release_scripts`, which packs `LICENSE` and
`THIRD-PARTY-NOTICES`).

`make ci` also shares the host with other heavy CI runs, such as Aether's
`make ci` and every Archon stage's: it first takes one of `CI_HOST_SLOTS`
(default 2) host-wide slots, a `flock` on
`${XDG_RUNTIME_DIR:-/tmp}/heavy-ci/slot-<i>.lock` for `i` in `0..N-1`.
When all are taken it says once that it waits, and waits until one frees.
The slot is released when `make ci` ends, whether it passes, fails or is
interrupted. `CI_HOST_SLOTS=0 make ci` skips the slots, and `make ci-checks`
runs the same checks without one. Without `flock` (util-linux) it runs
without a slot and says so. GitHub CI runs one job per runner and is
unaffected.

`make e2e` runs the mobile web app in headless Chromium against a real
archond (every screen's flows, pairing, revoke). It installs its npm packages
and the browser on first use and needs `node` and `npm`. It is not part of
`make ci`: CI runs it on the release pull request into `main`, and you should
run it when you change `crates/archon-remote`. Without a browser it prints
SKIP locally; with `CI` set it fails instead.

### CI on the release branch

Maintainers gather fixes on `next` and release it to `main` in one pull
request. A pull request into `next` runs only the `lint` job (format,
clippy and rustdoc); the `test` job is skipped on purpose. The suite runs
anyway on every push to `next`, so after each merge, and again on the pull
request from `next` into `main`, which must pass before anything reaches
`main`.

The trade-off: a merge into `next` can break the tests, which the push run
reports within minutes and before any release. In exchange each fix merges
without a full suite run, the slowest job CI has, and `next` is tested
once per merge rather than once per pull request update. So run
`make ci` yourself before you open a pull request into `next`, and fix a
red push run on `next` before you merge anything else.

More on the layout, the benchmark and local installs is in
[`docs/development.md`](docs/development.md).

## Live tests

Tests that need a live `aetherd`, real models or money are `#[ignore]`d and
gated on `ARCHON_LIVE=1`. CI never runs them. Run one on purpose, for example:

```sh
ARCHON_LIVE=1 cargo test -p archon-aether --test live -- --ignored
```

Point them at a sandbox Aether, never the daemon you work with.

## Benchmark

`bench/` compares Archon with plain Aether and other arms on hidden-graded
tasks. A dry run costs nothing:

```sh
cargo run -p archon-bench -- verify
cargo run -p archon-bench -- run --dry-run
```

See [`bench/README.md`](bench/README.md) for the method, isolation rules and
how to reproduce the published results.

## Code rules

- Keep functions under 60 lines, each with one clear responsibility.
- Give every non-trivial function at least two meaningful assertions or
  invariants, more at boundaries and state changes.
- Never swallow an error. Handle one only for a stated reason (add context,
  recover into a known-safe state, retry a transient failure), and keep the
  cause.
- `unsafe` is forbidden in this workspace.
- Keep dependencies few, well known and permissively licensed; add new ones
  to `[workspace.dependencies]`.
- Comment only what the code does not already say.
- Add tests that fail without your change. Test through public interfaces.

## Commits

- Use [Conventional Commits](https://www.conventionalcommits.org/) with a
  crate scope where it helps: `fix(daemon): ...`, `feat(cli): ...`,
  `docs(bench): ...`, `chore: ...`. Write the subject in the imperative
  and in lowercase, and explain the why in the body.
- Commit with your own git identity. Add no AI or tool attribution.

## Pull requests

Fork the repository, work on a branch, and open a pull request against
`main`. The pull request template asks for the same things as this list:

- **What and why.** Say what changed and why, and link the issue with
  `Fixes #<n>` when there is one.
- **Tests.** New behaviour and bug fixes come with tests. Say how you
  verified the change.
- **Green CI.** `make ci` passes. The first workflow run of a new
  contributor waits for a maintainer to approve it.
- **Review.** A maintainer looks at every pull request and is the one who
  merges it. Expect questions and requested changes; push follow-up commits
  to the same branch instead of force-pushing over reviewed work.
- Leave `CHANGELOG.md` to the maintainers, who write it at release time.

## Licence

Archon is released under the [MIT License](LICENSE). By submitting a
contribution you agree that it is licensed under the same terms (inbound =
outbound). There is no contributor licence agreement to sign.
