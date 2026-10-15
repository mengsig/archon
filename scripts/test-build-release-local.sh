#!/bin/sh
# Exercise orchestration and failure handling with explicit test tool doubles.
set -eu
root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "${XDG_CACHE_HOME:-$HOME/.cache}/archon-release-local"
testdir=$(mktemp -d "${XDG_CACHE_HOME:-$HOME/.cache}/archon-release-local/test.XXXXXXXX")
trap 'rm -rf "$testdir"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir -p "$testdir/repo/scripts" "$testdir/bin"
cp "$root/scripts/build-release-local.sh" "$root/scripts/package-release.sh" "$testdir/repo/scripts/"
cp "$root/LICENSE" "$root/THIRD-PARTY-NOTICES" "$testdir/repo/"
printf '[workspace.package]\nversion = "1.2.3"\n' > "$testdir/repo/Cargo.toml"
git -C "$testdir/repo" init -q
git -C "$testdir/repo" add .
git -C "$testdir/repo" commit -qm 'test fixture'
git -C "$testdir/repo" tag v1.2.3
git -C "$testdir/repo" tag v9.9.9
cat > "$testdir/bin/cargo" <<'EOF'
#!/bin/sh
set -eu
[ "$*" = "zigbuild --release --locked -p archon --target $7" ]
[ "$CARGO_PROFILE_RELEASE_STRIP/$CARGO_INCREMENTAL/$MACOSX_DEPLOYMENT_TARGET/$COPYFILE_DISABLE" = symbols/0/11.0/1 ]
case "$TMPDIR" in "$XDG_CACHE_HOME"/*) ;; *) exit 1 ;; esac
[ "$7" != "${FAIL_TARGET:-}" ] || exit 1
mkdir -p "$CARGO_TARGET_DIR/$7/release"
printf '#!/bin/sh\nexit 0\n' > "$CARGO_TARGET_DIR/$7/release/archon"
chmod +x "$CARGO_TARGET_DIR/$7/release/archon"
EOF
cat > "$testdir/bin/rustup" <<'EOF'
#!/bin/sh
printf '%s\n' x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin x86_64-apple-darwin
EOF
cat > "$testdir/bin/file" <<'EOF'
#!/bin/sh
printf '%s: %s\n' "$1" "${TEST_LINKAGE:-statically linked}"
EOF
printf '#!/bin/sh\nexit 0\n' > "$testdir/bin/zig"
cp "$testdir/bin/zig" "$testdir/bin/cargo-zigbuild"
chmod +x "$testdir/bin/"*
export PATH="$testdir/bin:$PATH" XDG_CACHE_HOME="$testdir/cache"
export CARGO_TARGET_DIR="$testdir/target"
script="$testdir/repo/scripts/build-release-local.sh"

expect_failure() {
    if sh "$script" "$1" "$testdir/$2" > "$testdir/log" 2>&1; then
        cat "$testdir/log" >&2
        exit 1
    fi
    [ ! -e "$testdir/$2/SHA256SUMS" ]
    [ ! -e "$testdir/$2/VERSION" ]
    grep -q "$3" "$testdir/log"
    [ "$(git -C "$testdir/repo" worktree list --porcelain | grep -c '^worktree ')" -eq 1 ]
    [ -z "$(ls -A "$XDG_CACHE_HOME/archon-release-local")" ]
}

expect_failure v9.9.9 mismatch 'is not the workspace version'
export FAIL_TARGET=aarch64-unknown-linux-musl
expect_failure v1.2.3 failed 'one or more targets failed'
[ -s "$testdir/failed/archon-x86_64-unknown-linux-musl.tar.gz" ]
unset FAIL_TARGET
export TEST_LINKAGE='dynamically linked'
expect_failure v1.2.3 dynamic 'is not statically linked'
[ ! -e "$testdir/dynamic/archon-x86_64-unknown-linux-musl.tar.gz" ]
unset TEST_LINKAGE
sh "$script" v1.2.3 "$testdir/success" > "$testdir/log" 2>&1
[ "$(cat "$testdir/success/VERSION")" = v1.2.3 ]
[ "$(wc -l < "$testdir/success/SHA256SUMS")" -eq 4 ]
(cd "$testdir/success" && sha256sum --check --strict SHA256SUMS)
if sh "$script" v1.2.3 "$testdir/success" > "$testdir/log" 2>&1; then exit 1; fi
grep -q 'output directory must be empty' "$testdir/log"
[ "$(git -C "$testdir/repo" worktree list --porcelain | grep -c '^worktree ')" -eq 1 ]
[ -z "$(ls -A "$XDG_CACHE_HOME/archon-release-local")" ]
printf 'local release orchestration tests passed\n'
