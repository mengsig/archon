#!/bin/sh
# Exercise host CI slots against a private runtime directory.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
slot=${CI_SLOT_UNDER_TEST:-"$root/scripts/ci-slot.sh"}
aether="$root/scripts/testdata/aether-ci-slot-0a44372a.sh"
if ! command -v flock >/dev/null 2>&1 || ! command -v timeout >/dev/null 2>&1; then
    echo "SKIP: test-ci-slot.sh needs flock and timeout"
    exit 0
fi
if [ "${CI_SLOT_TEST_SHELL:-}" = '' ]; then
    tested=
    for test_shell in bash dash; do
        if ! command -v "$test_shell" >/dev/null 2>&1; then
            echo "SKIP: $test_shell is not installed"
            continue
        fi
        echo "test-ci-slot.sh: testing $test_shell"
        CI_SLOT_TEST_SHELL=$test_shell timeout 90 "$test_shell" "$0"
        tested=1
    done
    if [ -z "$tested" ]; then CI_SLOT_TEST_SHELL='sh' timeout 90 sh "$0"; fi
    exit 0
fi
shell=$CI_SLOT_TEST_SHELL
export CI_SLOT_TEST_SHELL

testdir=$(mktemp -d)
holder=
waiter1=
waiter2=
waiter3=
cleanup() {
    touch "$testdir/release" "$testdir/release-again" "$testdir/marker-release"
    for pid in $holder $waiter1 $waiter2 $waiter3; do kill "$pid" 2>/dev/null || true; done
    for pid in $holder $waiter1 $waiter2 $waiter3; do wait "$pid" 2>/dev/null || true; done
    rm -rf "$testdir"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export XDG_RUNTIME_DIR="$testdir"
CARGO_BUILD_JOBS=2; export CARGO_BUILD_JOBS

fail() {
    echo "test-ci-slot.sh: $*" >&2
    exit 1
}

await_file() {
    tries=0
    until [ -e "$1" ]; do
        tries=$((tries + 1))
        [ "$tries" -le 200 ] || fail "timed out waiting for $1"
        sleep 0.1
    done
}

await_waiting() {
    tries=0
    until grep -q 'waiting for one to free' "$1" 2>/dev/null; do
        tries=$((tries + 1))
        [ "$tries" -le 200 ] || fail "timed out waiting for announcement in $1"
        sleep 0.1
    done
}

status=0
CI_HOST_SLOTS=1 "$shell" "$slot" "$shell" -c 'exit 3' || status=$?
[ "$status" -eq 3 ] || fail "the command's exit status $status is not passed on"
[ -e "$testdir/heavy-ci/slot-0.lock" ] || fail "no lock file for slot 0"
if CI_HOST_SLOTS=1 "$shell" "$slot" "$shell" -c 'echo >&9' 2>/dev/null; then
    fail "the command inherited the slot's lock descriptor"
fi
if CI_HOST_SLOTS=two "$shell" "$slot" true 2>/dev/null; then
    fail "a malformed CI_HOST_SLOTS was accepted"
fi

CI_HOST_SLOTS=1 "$shell" "$slot" timeout 30 "$shell" -c "touch '$testdir/held'; until [ -e '$testdir/release' ]; do sleep 0.1; done" &
holder=$!
await_file "$testdir/held"
CI_HOST_SLOTS=2 "$shell" "$slot" true 2>"$testdir/second.err"
[ ! -s "$testdir/second.err" ] || fail "a free second slot waited: $(cat "$testdir/second.err")"
CI_HOST_SLOTS=0 "$shell" "$slot" true

# shellcheck disable=SC2016
env -u CI_HOST_SLOTS "$shell" "$slot" "$shell" -c 'echo first >> "$1"' sh "$testdir/order" 2>"$testdir/waiter1.err" &
waiter1=$!
await_waiting "$testdir/waiter1.err"
# shellcheck disable=SC2016
CI_HOST_SLOTS=1 "$shell" "$aether" "$shell" -c 'echo killed >> "$1"' sh "$testdir/order" 2>"$testdir/waiter2.err" &
waiter2=$!
await_waiting "$testdir/waiter2.err"
# shellcheck disable=SC2016
CI_HOST_SLOTS=1 "$shell" "$aether" "$shell" -c 'echo last >> "$1"' sh "$testdir/order" 2>"$testdir/waiter3.err" &
waiter3=$!
await_waiting "$testdir/waiter3.err"
[ ! -e "$testdir/order" ] || fail "a waiter ran while the only slot was held"

tries=0
while :; do
    CI_HOST_SLOTS=1 timeout 5 "$shell" "$slot" --status >"$testdir/status" || fail "--status failed"
    grep -q "slot 0 holder: pid $holder, command" "$testdir/status" && break
    tries=$((tries + 1))
    [ "$tries" -le 50 ] || fail "status omitted the holder PID"
    sleep 0.1
done
grep -q "$waiter1" "$testdir/status" || fail "status omitted a waiter PID"
grep -qi 'holder' "$testdir/status" || fail "status omitted the holder"
grep -qi 'waiting since' "$testdir/status" || fail "status omitted waiting since"
grep -q "$testdir/order" "$testdir/status" || fail "status omitted the waiting command"

mkdir "$testdir/mock-bin"
cat > "$testdir/mock-bin/lslocks" <<'MOCK_LSLOCKS'
#!/bin/sh
printf '999999999 %s\n' "$XDG_RUNTIME_DIR/heavy-ci/slot-0.lock"
MOCK_LSLOCKS
chmod +x "$testdir/mock-bin/lslocks"
cat > "$testdir/mock-bin/ps" <<'MOCK_PS'
#!/bin/sh
if [ "$1" = -eo ]; then
    printf '%s 1 00:00:00 0.0\n%s %s 00:00:01 %s\n' "$MOCK_HOLDER" "$((MOCK_HOLDER + 10000000))" "$MOCK_HOLDER" "${MOCK_CPU:-80.0}"
elif [ "$4" = etimes= ]; then
    echo 301
elif [ "$4" = lstart= ]; then
    echo 'test start'
else
    exit 1
fi
MOCK_PS
chmod +x "$testdir/mock-bin/ps"
MOCK_HOLDER=$holder PATH="$testdir/mock-bin:$PATH" CI_HOST_SLOTS=1 timeout 5 "$shell" "$slot" --status >"$testdir/busy-status" || fail "status failed for busy descendant"
grep -q "pid $holder, command" "$testdir/busy-status" || fail "mock status omitted holder"
if grep -q 'idle holder' "$testdir/busy-status"; then fail "busy descendant reported idle"; fi
MOCK_CPU=0.0 MOCK_HOLDER=$holder PATH="$testdir/mock-bin:$PATH" CI_HOST_SLOTS=1 timeout 5 "$shell" "$slot" --status >"$testdir/idle-status" || fail "status failed for idle tree"
grep -q 'idle holder' "$testdir/idle-status" || fail "idle process tree was not reported"
cat > "$testdir/mock-bin/tr" <<'MOCK_TR'
#!/bin/sh
if [ "$1" = '\000' ]; then exit 1; fi
exec /usr/bin/tr "$@"
MOCK_TR
chmod +x "$testdir/mock-bin/tr"
MOCK_HOLDER=$holder PATH="$testdir/mock-bin:$PATH" CI_HOST_SLOTS=1 timeout 5 "$shell" "$slot" --status >"$testdir/race-status" || fail "status failed when process details disappeared"
grep -q 'unknown holder' "$testdir/race-status" || fail "missing holder details were not reported"

kill "$waiter2"
status=0
wait "$waiter2" || status=$?
waiter2=
[ "$status" -ne 0 ] || fail "killed waiter exited successfully"
touch "$testdir/release"
wait "$holder"
holder=
wait "$waiter1"
waiter1=
wait "$waiter3"
waiter3=
[ "$(cat "$testdir/order")" = "$(printf 'first\nlast')" ] || fail "waiters did not run in FIFO order after a waiter was killed"
[ "$(grep -c 'waiting' "$testdir/waiter1.err")" -eq 1 ] || fail "waiting was announced more than once"

# shellcheck disable=SC2016
CI_HOST_SLOTS=1 timeout 10 "$shell" "$slot" "$shell" -c '"$CI_SLOT_TEST_SHELL" "$1" "$CI_SLOT_TEST_SHELL" -c "exit 7"' "$shell" "$slot" && status=0 || status=$?
[ "$status" -eq 7 ] || fail "nested invocation did not run and preserve its exit status: $status"
for outer in "$slot" "$aether"; do
    for inner in "$slot" "$aether"; do
        # shellcheck disable=SC2016
        CI_HOST_SLOTS=1 timeout 10 "$shell" "$outer" "$shell" -c '"$CI_SLOT_TEST_SHELL" "$1" "$CI_SLOT_TEST_SHELL" -c "exit 7"' test "$inner" && status=0 || status=$?
        [ "$status" -eq 7 ] || fail "cross-client nested invocation failed: $outer -> $inner ($status)"
    done
done
cat > "$testdir/check-marker.sh" <<'CHECK_MARKER'
#!/bin/sh
set -eu
slot=$1
marker=$AETHER_CI_SLOT_HELD
owner=${marker%%:*}; rest=${marker#*:}; ticks=${rest%%:*}; lock=${rest#*:}
reject() {
    result=0
    AETHER_CI_SLOT_HELD=$1 timeout 1 "$CI_SLOT_TEST_SHELL" "$slot" true >/dev/null 2>&1 || result=$?
    [ "$result" -eq 124 ] || { echo "invalid marker bypassed the slot: $result" >&2; exit 1; }
}
reject "999999999:1:$lock"
reject "$owner:0:$lock"
reject "$owner:$ticks:$lock.foreign"
self_ticks=$(sed 's/.*) //' "/proc/$$/stat" | awk '{print $20}')
reject "$$:$self_ticks:$lock"
printf '%s\n' "$marker" > "$2"
until [ -e "$3" ]; do sleep 0.1; done
CHECK_MARKER
CI_HOST_SLOTS=1 "$shell" "$slot" timeout 15 "$shell" "$testdir/check-marker.sh" "$slot" "$testdir/marker" "$testdir/marker-release" &
holder=$!
await_file "$testdir/marker"
result=0
AETHER_CI_SLOT_HELD=$(cat "$testdir/marker") timeout 1 "$CI_SLOT_TEST_SHELL" "$slot" true >/dev/null 2>&1 || result=$?
[ "$result" -eq 124 ] || fail "non-descendant reused another holder's marker"
touch "$testdir/marker-release"
wait "$holder"
holder=
# shellcheck disable=SC2016
XDG_RUNTIME_DIR="$testdir/" CI_HOST_SLOTS=1 timeout 5 "$shell" "$slot" "$shell" -c '"$CI_SLOT_TEST_SHELL" "$1" true' "$shell" "$slot" || fail "nested call with trailing slash deadlocked"

# shellcheck disable=SC2016
CI_HOST_SLOTS=1 "$shell" "$aether" timeout 15 "$shell" -c 'touch "$1"; while [ ! -e "$2" ]; do sleep 0.1; done' sh "$testdir/held-again" "$testdir/release-again" &
holder=$!
await_file "$testdir/held-again"
CI_HOST_SLOTS=1 "$shell" "$aether" true 2>"$testdir/head.err" &
waiter1=$!
await_waiting "$testdir/head.err"
CI_HOST_SLOTS=1 "$shell" "$slot" touch "$testdir/after-head" 2>"$testdir/after-head.err" &
waiter2=$!
await_waiting "$testdir/after-head.err"
kill -KILL "$waiter1"
wait "$waiter1" 2>/dev/null || :
waiter1=
touch "$testdir/release-again"
wait "$holder"; holder=
await_file "$testdir/after-head"
wait "$waiter2"; waiter2=
CI_HOST_SLOTS=1 timeout 5 "$shell" "$slot" --status >"$testdir/empty-status" || fail "--status failed with no holders"
echo "test-ci-slot.sh: ok"
