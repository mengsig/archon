#!/bin/sh
# Packs one release archive: the archon binary and the licence files that
# travel with it. Usage: package-release.sh BINARY ARCHIVE
set -eu

MEMBERS="archon LICENSE THIRD-PARTY-NOTICES"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

main() {
    [ "$#" -eq 2 ] || die "usage: package-release.sh BINARY ARCHIVE"
    binary=$1
    archive=$2
    root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
    if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
        die "$binary is not an executable file"
    fi
    for notice in LICENSE THIRD-PARTY-NOTICES; do
        [ -s "$root/$notice" ] || die "$root/$notice is missing or empty"
    done

    staging=$(mktemp -d)
    trap 'rm -rf "$staging"' EXIT
    cp "$binary" "$staging/archon"
    chmod 755 "$staging/archon"
    cp "$root/LICENSE" "$root/THIRD-PARTY-NOTICES" "$staging/"
    chmod 644 "$staging/LICENSE" "$staging/THIRD-PARTY-NOTICES"
    # shellcheck disable=SC2086  # MEMBERS is a list of plain names
    tar -czf "$archive" -C "$staging" $MEMBERS

    packed=$(tar -tzf "$archive" | tr '\n' ' ')
    [ "$packed" = "$MEMBERS " ] || die "$archive holds '$packed', not '$MEMBERS'"
}

main "$@"
