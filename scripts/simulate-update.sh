#!/bin/sh
# End-to-end simulation of archon's self-update while a stage agent runs, in
# a private sandbox: its own HOME, XDG dirs, aetherd, archond, fake GitHub
# releases (http.server) and a fake OpenAI-compatible engine. Nothing outside
# the sandbox is used or touched besides reading the installed `aether`.
#
# Scenario A: a bare `archon` offers NEW at launch; "y" must reach the TUI in
#   under 10s, with archond restarted on NEW and the stage agent kept running.
# Scenario B: `archon update --yes` gets Ctrl+C while it restarts archond; it
#   must exit within 3s, leaving a working archon and archond, agent kept.
# Scenario F (with --base): the first update from a release before the fix,
#   its documented path: the base's `update --no-restart`, then NEW's
#   `update --yes`, which must restart the base archond in under 30s.
# shellcheck source-path=SCRIPTDIR
set -u

usage() {
    cat <<'EOF'
Usage: scripts/simulate-update.sh [--source DIR | --old BIN --new BIN] [--work DIR]
                                  [--out DIR] [--a-timeout SECS] [--base BIN] [--keep]

  --source DIR          the archon checkout OLD and NEW are built from (default:
                        the one this script is in); only its tracked files are
                        copied, uncommitted edits included, and it is not written

  --old BIN, --new BIN  use these archon binaries instead of building them;
                        NEW must report a higher version than OLD
  --work DIR            build directory (scratch tree, binaries; default
                        $TMPDIR/simulate-update-build); CARGO_TARGET_DIR, when
                        set, is the cargo target directory, else DIR/target
  --out DIR             copy transcripts, results and logs here before cleanup
  --a-timeout SECS      how long scenario A waits for the TUI after "y" (60)
  --base BIN            an archon of OLD's version from before the fix: also
                        run scenario F, the first update from such a release
  --keep                leave the sandbox in place (processes are still stopped)

Builds OLD from the source as it is and NEW with its workspace version bumped
one patch (cargo build -p archon, CARGO_BUILD_JOBS=4, debug).
Exit status: 0 when every check passed.
EOF
}

parse_args() {
    OLD_BIN='' NEW_BIN='' BASE_BIN='' OUT='' SOURCE='' KEEP=0 A_TIMEOUT=60
    WORK=${TMPDIR:-/tmp}/simulate-update-build
    while [ "$#" -gt 0 ]; do
        case $1 in
            --source) SOURCE=${2:?--source needs a directory}; shift 2 ;;
            --old) OLD_BIN=${2:?--old needs a binary}; shift 2 ;;
            --new) NEW_BIN=${2:?--new needs a binary}; shift 2 ;;
            --work) WORK=${2:?--work needs a directory}; shift 2 ;;
            --out) OUT=${2:?--out needs a directory}; shift 2 ;;
            --a-timeout) A_TIMEOUT=${2:?--a-timeout needs seconds}; shift 2 ;;
            --base) BASE_BIN=${2:?--base needs a binary}; shift 2 ;;
            --keep) KEEP=1; shift ;;
            -h | --help) usage; exit 0 ;;
            *) usage >&2; exit 2 ;;
        esac
    done
    if { [ -n "$OLD_BIN" ] && [ -z "$NEW_BIN" ]; } || { [ -z "$OLD_BIN" ] && [ -n "$NEW_BIN" ]; }; then
        echo "error: --old and --new go together" >&2
        exit 2
    fi
    case $A_TIMEOUT in "" | *[!0-9]*) echo "error: --a-timeout takes whole seconds" >&2; exit 2 ;; esac
    case $OUT in "" | /*) ;; *) OUT=$PWD/$OUT ;; esac
    if [ -n "$SOURCE" ] && [ -n "$OLD_BIN" ]; then
        echo "error: --source builds OLD and NEW; --old and --new skip the build" >&2
        exit 2
    fi
}

absolute_binary() {
    [ -f "$1" ] && [ -x "$1" ] || die "$1 is not an executable file"
    (cd "$(dirname "$1")" && printf '%s/%s\n' "$PWD" "$(basename "$1")")
}

# OLD_BIN, NEW_BIN and their versions, built unless given.
binaries() {
    if [ -z "$OLD_BIN" ]; then
        SOURCE=$(CDPATH='' cd -- "${SOURCE:-$TREE}" && pwd) || die "no source directory ${SOURCE:-$TREE}"
        [ -f "$SOURCE/Cargo.toml" ] && [ -d "$SOURCE/crates/archon" ] || die "$SOURCE is not an archon checkout"
        say "building from $SOURCE at $(git -C "$SOURCE" rev-parse --short HEAD)"
        TARGET_DIR=${CARGO_TARGET_DIR:-$WORK/target}
        build_binaries
    fi
    OLD_BIN=$(absolute_binary "$OLD_BIN")
    NEW_BIN=$(absolute_binary "$NEW_BIN")
    OLD_VERSION=$(version_of "$OLD_BIN") || die "$OLD_BIN --version failed"
    NEW_VERSION=$(version_of "$NEW_BIN") || die "$NEW_BIN --version failed"
    if [ -n "$BASE_BIN" ]; then
        BASE_BIN=$(absolute_binary "$BASE_BIN")
        base=$(version_of "$BASE_BIN") || die "$BASE_BIN --version failed"
        [ "$base" = "$OLD_VERSION" ] || die "--base reports $base; scenario F wants $OLD_VERSION"
    fi
    [ "$OLD_VERSION" != "$NEW_VERSION" ] || die "OLD and NEW both report $OLD_VERSION"
    say "OLD archon $OLD_VERSION ($OLD_BIN), NEW archon $NEW_VERSION ($NEW_BIN)"
}

summary() {
    say "summary"
    cat "$SUMMARY"
    for file in "$ROOT"/out/scenario-*.transcript; do
        [ -f "$file" ] || continue
        if [ -n "$OUT" ]; then
            echo "transcript: $OUT/$(basename "$file")"
        else
            echo "transcript: $file (removed at exit unless --keep or --out)"
        fi
    done
    if [ "$FAILED" = 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
}

main() {
    parse_args "$@"
    TREE=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
    HERE=$TREE/scripts/simulate-update
    # shellcheck source=simulate-update/lib.sh
    . "$HERE/lib.sh"
    # shellcheck source=simulate-update/setup.sh
    . "$HERE/setup.sh"
    # shellcheck source=simulate-update/scenarios.sh
    . "$HERE/scenarios.sh"
    FAILED=0
    binaries
    make_sandbox
    start_release_server
    publish_releases
    start_engine
    configure_archon
    register_mcp
    start_aetherd
    scenario_a
    scenario_b
    scenario_f
    summary
    [ "$FAILED" = 0 ]
}

main "$@"
