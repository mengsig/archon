#!/bin/sh
# Stage the binaries run.py expects under bench/results-release-1.6.7/.work.
# Usage, from the root of a checkout that has these scripts: sh stage.sh [candidate commit]
set -eu

CANDIDATE=${1:-acaad050}
WORK=$(pwd)/bench/results-release-1.6.7/.work
SOURCE=$WORK/candidate-source
ARCHIVE_SUFFIX=x86_64-unknown-linux-musl.tar.gz

test -f Cargo.toml && test -d bench/results-release-1.6.7 || {
    echo "run from the repository root" >&2
    exit 2
}

# fetch <repo> <tag> <binary> <dir>: download one release archive, check it
# against the release's SHA256SUMS, and unpack <binary> into .work/dl/<dir>.
fetch() {
    archive="$3-$ARCHIVE_SUFFIX"
    package="$WORK/dl/$4-package"
    if [ -e "$package" ] || [ -e "$WORK/dl/$4" ]; then
        echo "$WORK/dl/$4 already exists; remove it to stage again" >&2
        exit 1
    fi
    mkdir -p "$package" "$WORK/dl/$4"
    gh release download "$2" --repo "$1" --pattern "$archive" --pattern SHA256SUMS --dir "$package"
    grep -q "  $archive\$" "$package/SHA256SUMS" || {
        echo "$1 $2 SHA256SUMS has no line for $archive" >&2
        exit 1
    }
    (cd "$package" && grep "  $archive\$" SHA256SUMS | sha256sum --check --strict -)
    tar -xzf "$package/$archive" -C "$WORK/dl/$4" "$3"
    test -x "$WORK/dl/$4/$3"
    sha256sum "$package/$archive" "$WORK/dl/$4/$3"
}

commit=$(git rev-parse --verify "$CANDIDATE^{commit}")

fetch mengsig/archon v1.6.6 archon old-archon
fetch mengsig/aether v1.4.3 aether old-aether
fetch mengsig/aether v1.4.4 aether new-aether

# The measured candidate predates these scripts, so build it from an export of that commit.
if [ -e "$SOURCE" ]; then
    echo "$SOURCE already exists; remove it to stage again" >&2
    exit 1
fi
mkdir -p "$SOURCE"
git archive "$commit" | tar -x -C "$SOURCE"
TARGET=${CARGO_TARGET_DIR:-$SOURCE/target}
(cd "$SOURCE" && cargo build --release --locked -p archon -p archon-bench)
mkdir -p "$WORK/bin"
echo "candidate $commit"
cp "$TARGET/release/archon" "$WORK/bin/archon-new"
cp "$TARGET/release/archon-bench" "$WORK/bin/archon-bench"
sha256sum "$WORK/bin/archon-new" "$WORK/bin/archon-bench"

"$WORK/dl/old-archon/archon" --version
"$WORK/bin/archon-new" --version
"$WORK/dl/old-aether/aether" --version
"$WORK/dl/new-aether/aether" --version
