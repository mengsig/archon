#!/bin/sh
# Append "<UTC time> <load1> <load5> <load15>" to <log> once a minute until killed.
# Usage: sh loadlog.sh <log>
set -eu

test $# -eq 1 || {
    echo "usage: loadlog.sh <log>" >&2
    exit 2
}
test -r /proc/loadavg

while :; do
    printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$(cut -d' ' -f1-3 /proc/loadavg)" >> "$1"
    sleep 60
done
