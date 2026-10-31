# The scenarios of simulate-update.sh: the launch question answered
# "y" (A), `archon update --yes` cut by Ctrl+C mid-restart (B), and the
# first update from a release before the fix (F, with --base).
# Sourced after lib.sh and setup.sh.

STAGE_START_LIMIT=180
STAGE_FINISH_LIMIT=180
TUI_LIMIT=10

valid_id() {
    case $1 in
        "" | *[!A-Za-z0-9_]*) die "'$1' is not an id" ;;
    esac
}

# The running stage agent of ITEM, or nothing.
stage_agent() {
    valid_id "$1"
    sb aether agents --json 2>/dev/null | json "next((a['id'] for a in d
        if a.get('role', '').startswith('archon-stage:')
        and a['scope'].get('labels', {}).get('archon.item') == '$1'
        and a['status']['state'] == 'running'), None)"
}

agent_state() {
    valid_id "$1"
    sb aether agents --all --json 2>/dev/null \
        | json "next((a['status']['state'] for a in d if a['id'] == '$1'), 'missing')"
}

item_state() {
    valid_id "$1"
    archon_json queue list 2>/dev/null \
        | json "next((i['state']['state'] for i in d if i['id'] == '$1'), 'missing')"
}

daemon_field() {
    archon_json daemon status 2>/dev/null | json "(d.get('welcome') or {}).get('$1')"
}

has_stage_agent() { [ -n "$(stage_agent "$1")" ]; }
is_one_of() { [ "$1" = "$2" ] || [ "$1" = "$3" ]; }
new_pid_on() { [ "$1" = "$2" ] && [ "$3" != "$4" ]; }
item_succeeded() { [ "$(item_state "$1")" = succeeded ]; }
daemon_on() { [ "$(daemon_field daemon_version)" = "$1" ]; }
daemon_answers() { [ -n "$(daemon_field pid)" ]; }

# Install BIN as the sandbox's archon, atomically, as an update would.
install_archon() {
    cp "$1" "$ROOT/bin/archon.part" || die "copying $1 into the sandbox"
    mv "$ROOT/bin/archon.part" "$ROOT/bin/archon" || die "installing $1 in the sandbox"
}

# start_stage NAME: archond running, an item queued, its stage agent running.
# Sets ITEM, AGENT and DAEMON_PID.
start_stage() {
    DAEMON_PID=$(start_archond)
    if [ -z "$DAEMON_PID" ]; then
        check "$1: archond starts" FAIL "see archond-start.log"
        return 1
    fi
    ITEM=$(queue_stage "$1")
    if [ -z "$ITEM" ]; then
        check "$1: item queued" FAIL "see queue-add-$1.json"
        return 1
    fi
    wait_until "$STAGE_START_LIMIT" "the stage agent of $ITEM" has_stage_agent "$ITEM" || {
        check "$1: stage agent runs" FAIL "no running stage agent for $ITEM; see fake-model.jsonl"
        return 1
    }
    AGENT=$(stage_agent "$ITEM")
    check "$1: stage agent runs" OK "$AGENT on $ITEM under archond pid $DAEMON_PID"
}

# Release the held reply of NAME and wait for its item to succeed.
finish_stage() {
    touch "$ROOT/release/$1"
    started=$(now)
    if wait_until "$STAGE_FINISH_LIMIT" "item $ITEM to succeed" item_succeeded "$ITEM"; then
        check "$1: stage finishes" OK "$ITEM succeeded $(since "$started")s after the release, under archond $(daemon_field daemon_version) pid $(daemon_field pid)"
    else
        check "$1: stage finishes" FAIL "$ITEM is $(item_state "$ITEM") after ${STAGE_FINISH_LIMIT}s"
    fi
}

last_progress() {
    grep -a -e 'archon: ' -e 'Updated archon' -e 'interrupted' "$1" | tail -n 1 | cut -c1-200
}

check_agent_alive() {
    state=$(agent_state "$AGENT")
    case $state in
        running | waiting | created) check "$1: stage agent kept" OK "$AGENT is $state" ;;
        *) check "$1: stage agent kept" FAIL "$AGENT is $state" ;;
    esac
}

check_tui_time() {
    result=$1
    transcript=$2
    after=$(json 'd.get("tui_after_s")' <"$result")
    if [ -z "$after" ]; then
        check "A: TUI after y" FAIL "$(json 'd.get("failure")' <"$result"); last line: $(last_progress "$transcript")"
        grep -q 'restarting archond' "$transcript" && ! grep -q '1049h' "$transcript" \
            && say "scenario A stopped in archond's restart: a drain waiting for the running stage agent"
        return
    fi
    after=$(printf '%.3f' "$after")
    if python3 -c 'import sys; sys.exit(not float(sys.argv[1]) < float(sys.argv[2]))' "$after" "$TUI_LIMIT"; then
        check "A: TUI after y" OK "${after}s (limit ${TUI_LIMIT}s)"
    else
        check "A: TUI after y" FAIL "${after}s (limit ${TUI_LIMIT}s)"
    fi
}

scenario_a() {
    say "scenario A: a bare archon, its launch question answered y"
    start_stage A || return
    rm -f "$ROOT/archon-data/update-check.json"
    transcript=$ROOT/out/scenario-a.transcript
    result=$ROOT/out/scenario-a.json
    cd "$ROOT/home" || die "no sandbox home"
    # shellcheck disable=SC2086
    python3 "$HERE/pty_driver.py" launch --transcript "$transcript" --result "$result" \
        --tui-timeout "$A_TIMEOUT" -- env -i $SANDBOX_ENV archon
    cd "$TREE" || die "cannot return to $TREE"
    check_tui_time "$result" "$transcript"
    # A launch whose update fails opens the TUI of the old archon anyway.
    failed=$(grep -a -e 'update failed' -e 'did not finish cleanly' "$transcript" | head -n 1 | cut -c1-300)
    expect "A: update finished cleanly" "${failed:-no failure reported}" [ -z "$failed" ]
    installed=$(version_of "$ROOT/bin/archon")
    expect "A: installed archon" "reports $installed, want $NEW_VERSION" [ "$installed" = "$NEW_VERSION" ]
    wait_until 30 "archond on $NEW_VERSION" daemon_on "$NEW_VERSION"
    version=$(daemon_field daemon_version)
    pid=$(daemon_field pid)
    [ -n "$pid" ] && own_pid "$pid"
    expect "A: archond restarted" "runs $version pid $pid, was pid $DAEMON_PID; want $NEW_VERSION, a new pid" \
        new_pid_on "$version" "$NEW_VERSION" "$pid" "$DAEMON_PID"
    check_agent_alive A
    finish_stage A
}

stop_archond() {
    bounded 120 sb archon daemon stop >>"$ROOT/out/reset.log" 2>&1 \
        || bounded 60 sb archon daemon stop --force >>"$ROOT/out/reset.log" 2>&1
}

# Back to BIN: installed, and no archond running. A restart an earlier update
# asked for may start archond again once it stops, so stop until it stays down.
reset_to() {
    install_archon "$1"
    for round in 1 2 3; do
        stop_archond || die "archond would not stop for scenario B; see $ROOT/out/reset.log"
        sleep 5
        daemon_answers || return 0
        say "archond came back after stop $round ($(daemon_field daemon_version) pid $(daemon_field pid)); stopping it again"
    done
    die "archond keeps coming back after it stops; see $ROOT/out/reset.log"
}

check_interrupt() {
    result=$1
    after=$(json 'd.get("exit_after_s")' <"$result")
    failure=$(json 'd.get("failure")' <"$result")
    if [ -n "$failure" ]; then
        check "B: exit after Ctrl+C" FAIL "$failure"
    else
        check "B: exit after Ctrl+C" OK "${after}s, status $(json 'd.get("exit_status")' <"$result")"
    fi
}

scenario_b() {
    say "scenario B: archon update --yes, Ctrl+C while it restarts archond"
    reset_to "$OLD_BIN"
    start_stage B || return
    expect "B: archond on OLD" "runs $(daemon_field daemon_version)" daemon_on "$OLD_VERSION"
    transcript=$ROOT/out/scenario-b.transcript
    result=$ROOT/out/scenario-b.json
    # shellcheck disable=SC2086
    python3 "$HERE/pty_driver.py" interrupt --transcript "$transcript" --result "$result" \
        --exit-limit 3 -- env -i $SANDBOX_ENV archon update --yes
    check_interrupt "$result"
    installed=$(version_of "$ROOT/bin/archon")
    expect "B: installed archon runs" "reports '$installed', want $OLD_VERSION or $NEW_VERSION" \
        is_one_of "$installed" "$OLD_VERSION" "$NEW_VERSION"
    started=$(now)
    bounded 30 sb archon daemon start >>"$ROOT/out/scenario-b-start.log" 2>&1
    if wait_until 30 "archond to answer" daemon_answers; then
        pid=$(daemon_field pid)
        own_pid "$pid"
        check "B: archond answers" OK "$(daemon_field daemon_version) pid $pid after $(since "$started")s"
    else
        check "B: archond answers" FAIL "no answer to daemon status within 30s"
    fi
    check_agent_alive B
    finish_stage B
    check_interrupt_told "$transcript"
}

# What the interrupt said happens to archond must be what happens.
check_interrupt_told() {
    said=$(grep -a 'interrupted by' "$1" | cut -c1-300)
    case $said in
        *"may not have been asked"*)
            check "B: interrupt tells the truth" OK "archond runs $(daemon_field daemon_version): it says $said" ;;
        *"as it was asked"*)
            wait_until 30 "archond on $NEW_VERSION" daemon_on "$NEW_VERSION"
            expect "B: interrupt tells the truth" "archond runs $(daemon_field daemon_version) after: $said" \
                daemon_on "$NEW_VERSION" ;;
        *) check "B: interrupt tells the truth" FAIL "no interrupt message in $1" ;;
    esac
}

# Prefix each line of stdin with the seconds since it started.
stamp() {
    python3 -u -c 'import sys, time
start = time.monotonic()
for line in sys.stdin:
    print(f"{time.monotonic() - start:8.3f} | {line}", end="", flush=True)'
}

scenario_f() {
    [ -n "$BASE_BIN" ] || return 0
    say "scenario F: the first update from a release before the fix"
    reset_to "$BASE_BIN"
    start_stage F || return
    expect "F: archond on the base release" "runs $(daemon_field daemon_version)" daemon_on "$OLD_VERSION"
    transcript=$ROOT/out/scenario-f.transcript
    started=$(now)
    { bounded 120 sb archon update --no-restart 2>&1; echo "exit $?"; } | stamp >"$transcript"
    installed=$(version_of "$ROOT/bin/archon")
    expect "F: base swaps in NEW" "reports $installed, want $NEW_VERSION" [ "$installed" = "$NEW_VERSION" ]
    { bounded 120 sb archon update --yes 2>&1; echo "exit $?"; } | stamp >>"$transcript"
    took=$(since "$started")
    expect "F: both updates" "took ${took}s (limit 30s), $(tail -n 1 "$transcript")" \
        python3 -c 'import sys; sys.exit(not float(sys.argv[1]) < 30)' "$took"
    wait_until 30 "archond on $NEW_VERSION" daemon_on "$NEW_VERSION"
    pid=$(daemon_field pid)
    [ -n "$pid" ] && own_pid "$pid"
    expect "F: archond restarted" "runs $(daemon_field daemon_version) pid $pid, was pid $DAEMON_PID" \
        new_pid_on "$(daemon_field daemon_version)" "$NEW_VERSION" "$pid" "$DAEMON_PID"
    check_agent_alive F
    finish_stage F
}
