# Building the two archons and standing up the sandbox: fake releases, the
# fake engine, a sandbox aetherd and archond, and a running stage agent.
# Sourced by simulate-update.sh after lib.sh.

FAKE_PROVIDER=sim
FAKE_MODEL=sim-model

# bump_patch 1.4.8 -> 1.4.9
bump_patch() {
    printf '%s\n' "$1" | awk -F. 'NF == 3 { printf "%s.%s.%d\n", $1, $2, $3 + 1; ok = 1 } END { exit !ok }'
}

workspace_version() {
    awk '/^\[workspace.package\]/ { inside = 1; next }
         /^\[/ { inside = 0 }
         inside && /^version *=/ { gsub(/[" ]/, "", $0); sub(/^version=/, ""); print; exit }' "$1/Cargo.toml"
}

# A copy of every tracked file of TREE, uncommitted edits included.
copy_tracked() {
    tree=$1
    dest=$2
    mkdir -p "$dest"
    (cd "$tree" && git ls-files -z | tar --null -T - -cf -) | tar -xf - -C "$dest" \
        || die "copying the tracked files of $tree to $dest"
    [ -f "$dest/Cargo.toml" ] || die "$dest has no Cargo.toml after the copy"
}

# Set every workspace crate's version in TREE from OLD to NEW.
set_version() {
    tree=$1
    old=$2
    new=$3
    members=$(sed -n 's/^name = "\(.*\)"/\1/p' "$tree"/crates/*/Cargo.toml | sort -u | tr '\n' ' ')
    [ -n "$members" ] || die "no workspace crates found in $tree/crates"
    awk -v old="$old" -v new="$new" '
        /^\[workspace.package\]/ { inside = 1 }
        /^\[/ && !/^\[workspace.package\]/ { inside = 0 }
        inside && $0 == "version = \"" old "\"" { $0 = "version = \"" new "\""; done = 1 }
        { print } END { exit !done }' "$tree/Cargo.toml" >"$tree/Cargo.toml.new" \
        || die "no version = \"$old\" in [workspace.package] of $tree/Cargo.toml"
    mv "$tree/Cargo.toml.new" "$tree/Cargo.toml"
    awk -v old="$old" -v new="$new" -v members=" $members" '
        /^name = / { name = $3; gsub(/"/, "", name) }
        /^version = / && index(members, " " name " ") && $3 == "\"" old "\"" { $0 = "version = \"" new "\"" }
        { print }' "$tree/Cargo.lock" >"$tree/Cargo.lock.new"
    mv "$tree/Cargo.lock.new" "$tree/Cargo.lock"
    [ "$(workspace_version "$tree")" = "$new" ] || die "the version bump of $tree did not take"
}

# build_one TREE DEST: build archon in TREE and copy the binary to DEST.
build_one() {
    say "building archon in $1 (CARGO_TARGET_DIR=$TARGET_DIR)"
    (cd "$1" && CARGO_TARGET_DIR=$TARGET_DIR CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} \
        cargo build -p archon) || die "cargo build in $1 failed"
    cp "$TARGET_DIR/debug/archon" "$2" || die "copying the build of $1"
}

# OLD from a copy of SOURCE as it is, NEW from a copy bumped one patch; the
# copies keep cargo from writing into SOURCE (its Cargo.lock, say).
build_binaries() {
    old_version=$(workspace_version "$SOURCE")
    new_version=$(bump_patch "$old_version") || die "cannot bump '$old_version'"
    mkdir -p "$WORK/bin"
    rm -rf "$WORK/old-tree" "$WORK/new-tree"
    copy_tracked "$SOURCE" "$WORK/old-tree"
    build_one "$WORK/old-tree" "$WORK/bin/archon-old"
    copy_tracked "$SOURCE" "$WORK/new-tree"
    set_version "$WORK/new-tree" "$old_version" "$new_version"
    build_one "$WORK/new-tree" "$WORK/bin/archon-new"
    OLD_BIN=$WORK/bin/archon-old
    NEW_BIN=$WORK/bin/archon-new
}

# version_of BIN: the version `BIN --version` prints, without the name.
version_of() {
    out=$(bounded 30 "$1" --version) || return 1
    printf '%s\n' "${out#archon }"
}

make_sandbox() {
    RUN=''
    ROOT=$(mktemp -d "${TMPDIR:-/tmp}/simulate-update.XXXXXX") || die "mktemp failed"
    trap cleanup EXIT
    # Unix socket paths are limited to about 100 bytes: the runtime dir stays short.
    RUN=$(mktemp -d /tmp/simupd.XXXXXX) || die "mktemp of the runtime dir failed"
    trap 'exit 130' INT
    trap 'exit 143' TERM
    SUMMARY=$ROOT/out/summary.txt
    mkdir -p "$ROOT/bin" "$ROOT/home/.config/aether" "$ROOT/home/.local/share/aether" \
        "$ROOT/home/.local/state" "$ROOT/home/.cache" "$ROOT/tmp" \
        "$ROOT/archon-data" "$ROOT/archon-config" "$ROOT/www" "$ROOT/release" "$ROOT/out"
    chmod 700 "$RUN"
    : >"$SUMMARY"
    EXTRA_ENV=
    sandbox_env
    aether_bin=$(command -v aether) || die "no aether on PATH to copy"
    cp "$aether_bin" "$ROOT/bin/aether" || die "copying $aether_bin"
    # The receipt install.sh leaves; without it `aether update` refuses the copy.
    printf '%s\n' "# Written by Aether's install.sh; safe to delete together with aether." \
        "installer=install.sh" >"$ROOT/bin/.aether-receipt"
    cp "$OLD_BIN" "$ROOT/bin/archon" || die "copying $OLD_BIN"
    say "sandbox in $ROOT"
}

release_target() {
    case "$(uname -s) $(uname -m)" in
        "Linux x86_64") echo x86_64-unknown-linux-musl ;;
        "Linux aarch64" | "Linux arm64") echo aarch64-unknown-linux-musl ;;
        "Darwin x86_64") echo x86_64-apple-darwin ;;
        "Darwin arm64") echo aarch64-apple-darwin ;;
        *) die "no archon release target for $(uname -s) $(uname -m)" ;;
    esac
}

# The fake GitHub releases: NEW archon, and aether's own version only.
publish_releases() {
    target=$(release_target)
    archive=archon-$target.tar.gz
    dir=$ROOT/www/archon/download/v$NEW_VERSION
    mkdir -p "$dir" "$ROOT/www/archon/latest/download" "$ROOT/www/aether/latest/download"
    sh "$TREE/scripts/package-release.sh" "$NEW_BIN" "$dir/$archive" || die "packing $archive"
    (cd "$dir" && sha256sum "$archive" >"$archive.sha256") || die "hashing $archive"
    printf 'v%s\n' "$NEW_VERSION" >"$ROOT/www/archon/latest/download/VERSION"
    aether_version=$(sb aether --version | awk '{ print $NF }')
    [ -n "$aether_version" ] || die "the sandbox aether printed no version"
    printf 'v%s\n' "$aether_version" >"$ROOT/www/aether/latest/download/VERSION"
}

# start_server NAME CMD...: start CMD in the background as an owned process.
start_server() {
    name=$1
    shift
    # A plain command, not `sb`: a backgrounded function's $! would be a subshell.
    # shellcheck disable=SC2086
    env -i $SANDBOX_ENV "$@" >"$ROOT/out/$name.log" 2>&1 &
    own_pid "$!"
}

start_release_server() {
    start_server release-server python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$ROOT/www"
    wait_until 20 "the release server's port" grep -q 'port [0-9]' "$ROOT/out/release-server.log" \
        || die "the release server did not start: $(cat "$ROOT/out/release-server.log")"
    port=$(sed -n 's/.* port \([0-9]*\) .*/\1/p' "$ROOT/out/release-server.log" | head -n 1)
    [ -n "$port" ] || die "no port in the release server's log"
    EXTRA_ENV="ARCHON_INSTALL_BASE_URL=http://127.0.0.1:$port/archon"
    EXTRA_ENV="$EXTRA_ENV AETHER_INSTALL_BASE_URL=http://127.0.0.1:$port/aether"
    sandbox_env
    say "fake releases at http://127.0.0.1:$port"
}

start_engine() {
    start_server fake-model python3 "$HERE/fake_model.py" --port-file "$ROOT/model.port" \
        --release-dir "$ROOT/release" --log "$ROOT/out/fake-model.jsonl"
    wait_until 20 "the fake model's port" test -s "$ROOT/model.port" \
        || die "the fake model did not start: $(cat "$ROOT/out/fake-model.log")"
    MODEL_PORT=$(cat "$ROOT/model.port")
    cat >"$ROOT/home/.config/aether/providers.toml" <<EOF
[providers.$FAKE_PROVIDER]
name = "Simulated engine"
protocol = "openai-chat-completions"
base_url = "http://127.0.0.1:$MODEL_PORT/v1"
auth = { none = true }

[[providers.$FAKE_PROVIDER.add_models]]
id = "$FAKE_MODEL"
name = "Simulated model"
capabilities = { context_window = 65536, max_output_tokens = 8192, cost = { input_per_mtok = 0.0, output_per_mtok = 0.0 } }
EOF
    say "fake engine at http://127.0.0.1:$MODEL_PORT/v1"
}

# As in a shared-mode install: `archon setup` declares Archon's MCP server
# (the sandbox bin/archon) in the sandbox Aether's config.toml.
register_mcp() {
    bounded 60 sb archon --json setup >"$ROOT/out/archon-setup.json" 2>>"$ROOT/out/archon-setup.log" \
        || die "archon setup failed: $(cat "$ROOT/out/archon-setup.log")"
    written=$(json 'd["config"]' <"$ROOT/out/archon-setup.json")
    [ "$written" = "$ROOT/home/.config/aether/config.toml" ] \
        || die "archon setup wrote $written, not the sandbox Aether config"
    grep -q '^name = "archon"' "$written" || die "no archon MCP server in $written"
}

start_aetherd() {
    cd "$ROOT/home" || die "no sandbox home"
    start_server aetherd aether daemon --foreground
    cd "$TREE" || die "cannot return to $TREE"
    wait_until 60 "the sandbox aetherd" sb aether status || die "the sandbox aetherd did not start"
    sb aether models >"$ROOT/out/aether-models.txt" 2>&1
    grep -q "$FAKE_MODEL" "$ROOT/out/aether-models.txt" \
        || die "aetherd does not list $FAKE_PROVIDER/$FAKE_MODEL: $(cat "$ROOT/out/aether-models.txt")"
    sb aether mcp list >"$ROOT/out/aether-mcp.txt" 2>&1
    grep -q archon "$ROOT/out/aether-mcp.txt" \
        || die "aetherd has no archon MCP server: $(cat "$ROOT/out/aether-mcp.txt")"
}

configure_archon() {
    cat >"$ROOT/archon-config/config.toml" <<EOF
[aether]
mode = "shared"
socket = "$RUN/aetherd.sock"

[orchestrator]
model = "$FAKE_PROVIDER/$FAKE_MODEL"
wake_on_success = false

[limits]
min_free_gb = 0

[models]
allow = ["$FAKE_PROVIDER/$FAKE_MODEL"]

[update]
check_on_launch = true
cache_for = "1h"
EOF
    repo=$ROOT/home/simrepo
    mkdir -p "$repo"
    printf '[user]\n\tname = Update Simulation\n\temail = simulate-update@localhost\n' \
        >"$ROOT/home/.gitconfig"
    (cd "$repo" && sb git init -q -b main && echo sim >README && sb git add README \
        && sb git commit -qm init) || die "creating the throwaway repository"
}

try_start_archond() {
    bounded 60 sb archon daemon start >>"$ROOT/out/archond-start.log" 2>&1
}

# start_archond: start it, register the repository once, record its pid.
start_archond() {
    # Retried: an archond that was just stopped may still hold its log's lock.
    wait_until 60 "archon daemon start to succeed" try_start_archond \
        || die "archon daemon start failed: $(grep '^archon' "$ROOT/out/archond-start.log" | tail -n 3)"
    pid=$(archon_json daemon status | json 'd["welcome"]["pid"]')
    [ -n "$pid" ] || die "archon daemon status names no pid"
    own_pid "$pid"
    if [ ! -e "$ROOT/repo-added" ]; then
        bounded 60 sb archon repo add --ship local --name simrepo "$ROOT/home/simrepo" \
            >>"$ROOT/out/archond-start.log" 2>&1 || die "archon repo add failed"
        touch "$ROOT/repo-added"
    fi
    printf '%s\n' "$pid"
}

# queue_stage NAME: an errand item whose stage agent holds until release/NAME.
queue_stage() {
    printf 'Simulated work, held by the fake engine. SIMSTAGE-%s\n' "$1" >"$ROOT/brief-$1.md"
    printf '{"errand": [{"model": "%s/%s"}]}\n' "$FAKE_PROVIDER" "$FAKE_MODEL" >"$ROOT/plan.json"
    archon_json queue add --title "simulated stage $1" --pipeline errand \
        --brief-file "$ROOT/brief-$1.md" --model-plan-file "$ROOT/plan.json" \
        >"$ROOT/out/queue-add-$1.json" || die "archon queue add failed: $(cat "$ROOT/out/queue-add-$1.json")"
    item=$(json 'd.get("id") or d.get("item", {}).get("id")' <"$ROOT/out/queue-add-$1.json")
    [ -n "$item" ] || die "archon queue add named no item: $(cat "$ROOT/out/queue-add-$1.json")"
    printf '%s\n' "$item"
}
