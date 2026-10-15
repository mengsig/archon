#!/bin/sh
# Build the release workflow's assets from a tag, without GitHub Actions.
set -eu

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    result=$?
    trap - EXIT
    if [ -d "$scratch/source" ]; then
        git -C "$root" worktree remove --force "$scratch/source" || result=1
    fi
    rm -rf "$scratch" || result=1
    exit "$result"
}

build_target() {
    target=$1
    printf '\nBuilding %s\n' "$target"
    # zigbuild supplies both the Rust linker and SQLite's C cross compiler;
    # internally it runs cargo build with these same release arguments.
    cargo zigbuild --release --locked -p archon --target "$target" || {
        printf 'error: build failed for %s (Darwin cross builds may require SDKROOT pointing to an Apple SDK)\n' "$target" >&2
        return 1
    }
    binary="$CARGO_TARGET_DIR/$target/release/archon"
    description=$(file "$binary") || return 1
    printf '%s\n' "$description"
    case "$target" in
        *-linux-musl)
            printf '%s\n' "$description" | grep -Eq 'static(ally|-pie) linked' || {
                printf 'error: %s is not statically linked\n' "$binary" >&2
                return 1
            } ;;
    esac
    asset="archon-$target.tar.gz"
    sh scripts/package-release.sh "$binary" "$scratch/$asset" || return 1
    tar -tvzf "$scratch/$asset" || return 1
    mv "$scratch/$asset" "$outdir/$asset" || return 1
    (cd "$outdir" && sha256sum "$asset" > "$asset.sha256") || return 1
    printf '%s: %s bytes\n' "$asset" "$(wc -c < "$outdir/$asset")"
}

[ "$#" -eq 2 ] || die 'usage: build-release-local.sh <tag> <outdir>'
tag=$1
printf '%s\n' "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' || die "invalid release tag: $tag"
for tool in git cargo cargo-zigbuild rustup zig file tar sha256sum mktemp; do
    command -v "$tool" >/dev/null 2>&1 || die "missing prerequisite: $tool (install cargo-zigbuild with cargo install cargo-zigbuild --locked)"
done
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
caller=$(pwd)
git -C "$root" rev-parse --verify "refs/tags/$tag^{commit}" >/dev/null || die "tag not found: $tag"
mkdir -p "$2"
outdir=$(CDPATH='' cd -- "$2" && pwd)
[ -z "$(ls -A "$outdir")" ] || die "output directory must be empty: $outdir"
cache=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$cache/archon-release-local"
cache=$(CDPATH='' cd -- "$cache" && pwd)
scratch=$(mktemp -d "$cache/archon-release-local/build.XXXXXXXX")
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
git -C "$root" worktree add --detach "$scratch/source" "refs/tags/$tag"
cd "$scratch/source"
version=$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[ "$tag" = "v$version" ] || die "tag $tag is not the workspace version, v$version"
targets='x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin'
installed=$(rustup target list --installed)
for target in $targets; do
    printf '%s\n' "$installed" | grep -qx "$target" || die "missing Rust target: run rustup target add $target in the tagged checkout's toolchain"
done
# Relative target directories belong to the caller, not the tagged checkout.
case ${CARGO_TARGET_DIR:-} in
    '') CARGO_TARGET_DIR="$scratch/source/target" ;;
    /*) ;;
    *) CARGO_TARGET_DIR="$caller/$CARGO_TARGET_DIR" ;;
esac
export CARGO_TARGET_DIR
export CARGO_PROFILE_RELEASE_STRIP=symbols CARGO_INCREMENTAL=0
export MACOSX_DEPLOYMENT_TARGET=11.0 COPYFILE_DISABLE=1
# package-release.sh and compiler scratch files must also avoid /tmp.
mkdir "$scratch/tmp"
export TMPDIR="$scratch/tmp"
failed=0
for target in $targets; do
    build_target "$target" || failed=1
done
[ "$failed" -eq 0 ] || die 'one or more targets failed; SHA256SUMS and VERSION were not written'
(
    cd "$outdir"
    cat archon-*.tar.gz.sha256 > "$scratch/SHA256SUMS"
    [ "$(wc -l < "$scratch/SHA256SUMS")" -eq 4 ] || die 'expected exactly four checksums'
    sha256sum --check --strict "$scratch/SHA256SUMS"
)
printf '%s\n' "$tag" > "$scratch/VERSION"
mv "$scratch/SHA256SUMS" "$outdir/SHA256SUMS"
mv "$scratch/VERSION" "$outdir/VERSION"
