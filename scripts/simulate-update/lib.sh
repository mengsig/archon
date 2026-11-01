# Shared helpers of simulate-update.sh: reporting, bounded waits, the
# sandbox environment and the processes the simulation owns. Sourced, not run.

say() {
    printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*" >&2
}

die() {
    say "error: $*"
    exit 1
}

# check NAME OK|FAIL DETAIL: one line of the PASS/FAIL summary.
check() {
    case $2 in
        OK) verdict=PASS ;;
        *) verdict=FAIL; FAILED=1 ;;
    esac
    printf '%s  %s: %s\n' "$verdict" "$1" "$3" | tee -a "$SUMMARY" >&2
}

# info NAME DETAIL: a summary line that passes or fails nothing.
info() {
    printf 'INFO  %s: %s\n' "$1" "$2" | tee -a "$SUMMARY" >&2
}

# expect NAME DETAIL CMD...: PASS when CMD succeeds.
expect() {
    name=$1
    detail=$2
    shift 2
    if "$@"; then check "$name" OK "$detail"; else check "$name" FAIL "$detail"; fi
}

now() {
    python3 -c 'import time; print(f"{time.monotonic():.3f}")'
}

since() {
    python3 -c "import sys, time; print(f'{time.monotonic() - float(sys.argv[1]):.1f}')" "$1"
}

# bounded SECS CMD...: run CMD, killed after SECS; its status, 124 on timeout.
bounded() {
    secs=$1
    shift
    [ "$secs" -gt 0 ] || die "bounded needs a positive limit, not '$secs'"
    "$@" &
    bounded_pid=$!
    bounded_mark=${TMPDIR:-/tmp}/simulate-update.bounded.$$.$bounded_pid
    (sleep "$secs" && kill "$bounded_pid" 2>/dev/null && touch "$bounded_mark") \
        >/dev/null 2>&1 &
    bounded_watch=$!
    wait "$bounded_pid"
    bounded_status=$?
    kill "$bounded_watch" 2>/dev/null
    if [ -e "$bounded_mark" ]; then
        rm -f "$bounded_mark"
        return 124
    fi
    return "$bounded_status"
}

# wait_until SECS WHAT CMD...: poll CMD each second until it succeeds.
wait_until() {
    secs=$1
    what=$2
    shift 2
    deadline=$(($(date +%s) + secs))
    while ! "$@" >/dev/null 2>&1; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            say "timed out after ${secs}s waiting for $what"
            return 1
        fi
        sleep 1
    done
}

# json EXPR: evaluate a Python expression over the JSON on stdin, bound as `d`.
json() {
    python3 -c 'import json, sys
d = json.load(sys.stdin)
value = eval(sys.argv[1])
print("" if value is None else value)' "$1"
}

# sb CMD...: run CMD with only the sandbox's environment.
sb() {
    # shellcheck disable=SC2086  # SANDBOX_ENV holds VAR=value words without spaces
    env -i $SANDBOX_ENV "$@"
}

archon_json() {
    bounded 60 sb archon --json "$@"
}

# The environment every sandboxed command gets, and nothing else.
sandbox_env() {
    case "$ROOT$RUN" in *' '*) die "the sandbox path '$ROOT' contains a space" ;; esac
    SANDBOX_ENV="HOME=$ROOT/home USER=${USER:-sim} LOGNAME=${USER:-sim} SHELL=/bin/sh
TERM=xterm-256color LANG=C.UTF-8 TMPDIR=$ROOT/tmp PATH=$ROOT/bin:/usr/local/bin:/usr/bin:/bin
XDG_CONFIG_HOME=$ROOT/home/.config XDG_DATA_HOME=$ROOT/home/.local/share
XDG_STATE_HOME=$ROOT/home/.local/state XDG_CACHE_HOME=$ROOT/home/.cache
XDG_RUNTIME_DIR=$RUN ARCHON_DATA_DIR=$ROOT/archon-data
ARCHON_CONFIG_DIR=$ROOT/archon-config ARCHON_SOCKET=$RUN/archond.sock
AETHER_SOCKET=$RUN/aetherd.sock AETHER_CONFIG_DIR=$ROOT/home/.config/aether
AETHER_DATA_DIR=$ROOT/home/.local/share/aether ARCHON_NO_WIZARD=1
SIMULATE_UPDATE_ROOT=$ROOT $EXTRA_ENV"
    SANDBOX_ENV=$(printf '%s' "$SANDBOX_ENV" | tr '\n' ' ')
    socket="$RUN/aetherd.sock"
    [ "${#socket}" -lt 100 ] || die "the socket path $socket is too long for a Unix socket"
}

# Record a process the simulation started, so cleanup stops only its own.
own_pid() {
    [ -n "$1" ] || die "no pid to record"
    printf '%s\n' "$1" >>"$ROOT/pids"
}

# Whether PID still runs and is one of this sandbox's (by its environment).
sandbox_pid() {
    kill -0 "$1" 2>/dev/null || return 1
    [ -r "/proc/$1/environ" ] || return 0
    tr '\0' '\n' 2>/dev/null <"/proc/$1/environ" | grep -qxF "SIMULATE_UPDATE_ROOT=$ROOT"
}

cleanup() {
    status=$?
    trap - EXIT INT TERM
    [ -n "${ROOT:-}" ] && [ -d "$ROOT" ] || exit "$status"
    say "cleaning up $ROOT"
    if [ -x "$ROOT/bin/archon" ]; then
        bounded 30 sb archon daemon stop --force >/dev/null 2>&1
    fi
    if [ -x "$ROOT/bin/aether" ]; then
        bounded 30 sb aether stop >/dev/null 2>&1
    fi
    if [ -f "$ROOT/pids" ]; then
        while read -r pid; do
            sandbox_pid "$pid" && kill "$pid" 2>/dev/null
        done <"$ROOT/pids"
        sleep 1
        while read -r pid; do
            sandbox_pid "$pid" && kill -9 "$pid" 2>/dev/null
        done <"$ROOT/pids"
    fi
    [ -n "${OUT:-}" ] && copy_out
    [ "${KEEP:-0}" = 1 ] && { say "kept the sandbox in $ROOT and ${RUN:-}"; exit "$status"; }
    chmod -R u+w "$ROOT" 2>/dev/null
    rm -rf "$ROOT" ${RUN:+"$RUN"}
    exit "$status"
}

copy_out() {
    mkdir -p "$OUT" || return
    for file in "$ROOT"/out/* "${RUN:-$ROOT/none}"/*.log "$ROOT/archon-data/logs"/*.log; do
        [ -f "$file" ] && cp "$file" "$OUT/"
    done
    say "transcripts and logs copied to $OUT"
}
