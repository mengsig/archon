#!/bin/sh
# Installs a released archon without sudo. Options are environment variables,
# listed in docs/install.md; the body runs only from the last line.
set -eu

ARCHON_REPOSITORY="mengsig/archon"
AETHER_REPOSITORY="mengsig/aether"
# Every binary a release archive ships; archond is `archon daemon run`.
BINARIES="archon"

EXIT_FAILED=1
EXIT_AETHER_FAILED=2

STAGING=""
DOWNLOADER=""
INSTALL_DIR=""
AETHER_INSTALL_URL=""
AETHER_STEP_FAILED=0

say() {
    printf '%s\n' "$*"
}

warn() {
    printf 'warning: %s\n' "$*" >&2
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit "$EXIT_FAILED"
}

has() {
    command -v "$1" >/dev/null 2>&1
}

cleanup() {
    [ -z "$STAGING" ] || rm -rf "$STAGING"
}

# Prints the release target for this machine, e.g. x86_64-unknown-linux-musl.
detect_target() {
    os=$(uname -s)
    cpu=$(uname -m)
    case $os in
    Linux) system=unknown-linux-musl ;;
    Darwin)
        system=apple-darwin
        # An x86_64 shell under Rosetta still runs on Apple Silicon.
        if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
            cpu=arm64
        fi
        ;;
    *) die "unsupported operating system: $os. Archon is released for Linux and macOS; on Windows use WSL." ;;
    esac
    case $cpu in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) die "unsupported CPU: $cpu. Archon is released for x86_64 and arm64; build it from source instead (docs/install.md)." ;;
    esac
    printf '%s-%s\n' "$arch" "$system"
}

# Prints the release tag to install, or nothing for the latest release.
requested_version() {
    version=${ARCHON_VERSION:-}
    [ -n "$version" ] || return 0
    case $version in
    v*) ;;
    *) version="v$version" ;;
    esac
    printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' ||
        die "ARCHON_VERSION must look like v1.2.3, not '$ARCHON_VERSION'"
    printf '%s\n' "$version"
}

# Prints the absolute directory the binaries go to.
resolve_install_dir() {
    dir=${ARCHON_INSTALL_DIR:-}
    if [ -z "$dir" ]; then
        [ -n "${HOME:-}" ] || die "HOME is not set; set ARCHON_INSTALL_DIR to the directory to install to"
        dir="$HOME/.local/bin"
    fi
    case $dir in
    /*) ;;
    *) dir="$(pwd)/$dir" ;;
    esac
    printf '%s\n' "${dir%/}"
}

# Prints where release assets are downloaded from, without a trailing slash.
release_base_url() {
    base=${ARCHON_INSTALL_BASE_URL:-https://github.com/$ARCHON_REPOSITORY/releases}
    case $base in
    https://* | http://*) ;;
    *) die "ARCHON_INSTALL_BASE_URL must start with https:// or http://, not '$base'" ;;
    esac
    printf '%s\n' "${base%/}"
}

choose_downloader() {
    if has curl; then
        DOWNLOADER=curl
    elif has wget; then
        DOWNLOADER=wget
    else
        die "neither curl nor wget was found; install one of them and run this again"
    fi
}

# fetch URL FILE: both tools honour https_proxy and http_proxy themselves.
fetch() {
    url=$1
    dest=$2
    if [ "$DOWNLOADER" = curl ]; then
        set -- -fsSL --retry 2 --connect-timeout 20 --max-time 900
        case $url in
        https://*) set -- "$@" --proto '=https' --tlsv1.2 ;;
        esac
        curl "$@" -o "$dest" "$url"
    else
        wget -q -T 30 -O "$dest" "$url"
    fi
}

require_sha256_tool() {
    has sha256sum || has shasum ||
        die "neither sha256sum nor shasum was found; the download cannot be verified, so nothing was installed"
}

sha256_of() {
    if has sha256sum; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

is_sha256() {
    [ "${#1}" -eq 64 ] || return 1
    case $1 in
    *[!0-9a-f]*) return 1 ;;
    esac
}

# verify_checksum ARCHIVE SUMFILE: SUMFILE is `<sha256>  <name>`.
verify_checksum() {
    expected=$(head -n 1 "$2" | cut -d ' ' -f 1 | tr 'A-F' 'a-f')
    is_sha256 "$expected" || die "the published checksum is not a SHA-256: '$expected'. Nothing was installed."
    actual=$(sha256_of "$1")
    is_sha256 "$actual" || die "could not compute the SHA-256 of the download. Nothing was installed."
    [ "$expected" = "$actual" ] ||
        die "checksum mismatch: expected $expected, downloaded $actual. Nothing was installed; run this again, and report it if it repeats."
}

# download_release TARGET VERSION: leaves the verified archive in $STAGING.
download_release() {
    asset="archon-$1.tar.gz"
    base=$(release_base_url)
    if [ -n "$2" ]; then
        url="$base/download/$2/$asset"
    else
        url="$base/latest/download/$asset"
    fi
    say "Downloading $url"
    fetch "$url" "$STAGING/$asset" ||
        die "could not download $url. Check the version and your network; nothing was installed."
    fetch "$url.sha256" "$STAGING/$asset.sha256" ||
        die "could not download $url.sha256. Nothing was installed."
    verify_checksum "$STAGING/$asset" "$STAGING/$asset.sha256"
    say "Verified its SHA-256."
}

# unpack_release TARGET: extracts only the shipped binaries, then runs one.
unpack_release() {
    mkdir "$STAGING/unpacked"
    for binary in $BINARIES; do
        tar -xzf "$STAGING/archon-$1.tar.gz" -C "$STAGING/unpacked" "$binary" ||
            die "the archive has no $binary. Nothing was installed."
        if [ -L "$STAGING/unpacked/$binary" ] || [ ! -f "$STAGING/unpacked/$binary" ]; then
            die "$binary in the archive is not a regular file. Nothing was installed."
        fi
        chmod 755 "$STAGING/unpacked/$binary"
    done
    "$STAGING/unpacked/archon" --version >/dev/null 2>&1 ||
        die "the downloaded archon does not run on this machine ($1). Nothing was installed."
}

# Renames each binary over the installed one: same directory, so atomic.
install_binaries() {
    for binary in $BINARIES; do
        mv -f "$STAGING/unpacked/$binary" "$INSTALL_DIR/$binary" ||
            die "could not write $INSTALL_DIR/$binary"
    done
}

installed_version() {
    "$INSTALL_DIR/archon" --version 2>/dev/null || true
}

report_install() {
    new=$(installed_version)
    [ -n "$new" ] || die "$INSTALL_DIR/archon was installed but does not run"
    if [ -z "$1" ]; then
        say "Installed $new to $INSTALL_DIR."
    elif [ "$1" = "$new" ]; then
        say "Reinstalled $new in $INSTALL_DIR."
    else
        say "Replaced $1 with $new in $INSTALL_DIR."
    fi
}

# Prints the aether program to use, or nothing when there is none.
find_aether() {
    if has aether; then
        command -v aether
    elif [ -x "$INSTALL_DIR/aether" ]; then
        printf '%s\n' "$INSTALL_DIR/aether"
    fi
}

# number_after KEY FILE: the integer on the line `KEY <integer>`.
number_after() {
    sed -n "s/^$1 \([0-9][0-9]*\)\$/\1/p" "$2" | head -n 1
}

# schema_number KEY FILE: a top-level integer of `aether schema`.
schema_number() {
    sed -n "s/^  \"$1\": \([0-9][0-9]*\),\{0,1\}\$/\1/p" "$2" | head -n 1
}

aether_retry_command() {
    printf 'curl -fsSL %s | AETHER_INSTALL_DIR="%s" sh\n' "$AETHER_INSTALL_URL" "$INSTALL_DIR"
}

# report_aether PROGRAM: its version, and whether this archon can use it.
report_aether() {
    version=$("$1" --version 2>/dev/null | head -n 1) || version=""
    say "Aether: ${version:-unknown version} at $1."
    "$INSTALL_DIR/archon" update --requirements >"$STAGING/requirements" 2>/dev/null || true
    "$1" schema >"$STAGING/aether-schema" 2>/dev/null || true
    need_min=$(number_after aether_protocol_min "$STAGING/requirements")
    need_max=$(number_after aether_protocol_max "$STAGING/requirements")
    have_min=$(schema_number min_protocol_version "$STAGING/aether-schema")
    have_max=$(schema_number protocol_version "$STAGING/aether-schema")
    if [ -z "$need_min" ] || [ -z "$need_max" ] || [ -z "$have_min" ] || [ -z "$have_max" ]; then
        warn "could not compare this Aether with what Archon needs; run 'archon doctor' to check."
        return 0
    fi
    say "Archon needs an Aether that speaks protocol $need_min to $need_max; this one speaks $have_min to $have_max."
    if [ "$have_max" -lt "$need_min" ]; then
        warn "this Aether is too old for Archon. Update it with 'aether update', or: $(aether_retry_command)"
    elif [ "$have_min" -gt "$need_max" ]; then
        warn "this Aether is newer than this Archon supports. Update Archon with 'archon update'."
    fi
}

# Prints why no question can be asked, or nothing when one can.
no_question_reason() {
    if [ "${ARCHON_NONINTERACTIVE:-0}" = 1 ]; then
        printf 'ARCHON_NONINTERACTIVE=1\n'
    elif ! (: </dev/tty >/dev/tty) 2>/dev/null; then
        printf 'there is no terminal to ask on\n'
    fi
}

# confirm QUESTION: asks on the terminal; anything but "n" means yes.
confirm() {
    printf '%s [Y/n] ' "$1" >/dev/tty
    answer=""
    read -r answer </dev/tty || true
    case $answer in
    [Nn]*) return 1 ;;
    esac
}

# confirm_no QUESTION: asks on the terminal; only "y" means yes.
confirm_no() {
    printf '%s [y/N] ' "$1" >/dev/tty
    answer=""
    read -r answer </dev/tty || true
    case $answer in
    [Yy]*) return 0 ;;
    esac
    return 1
}

run_remote_setup() {
    if "$INSTALL_DIR/archon" remote setup "$@"; then
        return 0
    fi
    warn "the mobile web app is not set up. Archon is installed; to retry, run:"
    warn "  archon remote setup"
}

# The optional mobile web app (docs/remote-setup.md): ARCHON_REMOTE=1 sets it
# up without asking, ARCHON_REMOTE=0 skips it, and a terminal is asked.
remote_step() {
    case ${ARCHON_REMOTE:-} in
    0) return 0 ;;
    1)
        say "Setting up the mobile web app (ARCHON_REMOTE=1)."
        run_remote_setup --yes </dev/null
        return 0
        ;;
    esac
    [ -z "$(no_question_reason)" ] || return 0
    say "Archon can serve a mobile web app, published to your tailnet with 'tailscale serve'."
    if confirm_no "Set it up now?"; then
        run_remote_setup </dev/tty
    else
        say "Not now. 'archon remote setup' does it later."
    fi
}

run_aether_installer() {
    fetch "$AETHER_INSTALL_URL" "$STAGING/aether-install.sh" || return 1
    AETHER_INSTALL_DIR="$INSTALL_DIR" sh "$STAGING/aether-install.sh" </dev/null || return 1
    [ -x "$INSTALL_DIR/aether" ] || has aether
}

install_aether() {
    if run_aether_installer; then
        report_aether "$(find_aether)"
        return 0
    fi
    AETHER_STEP_FAILED=1
    warn "Aether could not be installed. Archon is installed and stays in place; to retry, run:"
    warn "  $(aether_retry_command)"
}

aether_step() {
    aether=$(find_aether)
    if [ -n "$aether" ]; then
        report_aether "$aether"
        return 0
    fi
    say "Archon runs its agents on the Aether daemon, and no 'aether' was found."
    reason=$(no_question_reason)
    if [ "${ARCHON_SKIP_AETHER:-0}" = 1 ]; then
        say "Not installing it (ARCHON_SKIP_AETHER=1). Install it before you use Archon:"
        say "  $(aether_retry_command)"
    elif [ -n "$reason" ]; then
        say "Installing Aether to $INSTALL_DIR without asking ($reason)."
        install_aether
    elif confirm "Install Aether to $INSTALL_DIR now?"; then
        install_aether
    else
        say "Not installing it. Install it before you use Archon:"
        say "  $(aether_retry_command)"
    fi
}

# missing NAME NEED PURPOSE HOW: one line for a program that is not installed.
missing() {
    say "  $1 ($2: $3): $4"
}

report_prerequisites() {
    say "Other programs Archon uses:"
    found_all=1
    has git || {
        found_all=0
        missing git required "a worktree for every item" "https://git-scm.com/install/ or 'brew install git'"
    }
    has claude || has codex || {
        found_all=0
        missing "claude or codex" "one required" "the engine that runs agents, logged in" \
            "Claude Code: 'curl -fsSL https://claude.ai/install.sh | bash'; Codex: 'npm install -g @openai/codex'"
    }
    has gh || {
        found_all=0
        missing gh optional "the pr ship mode and the GitHub watcher" \
            "https://cli.github.com/ or 'brew install gh', then 'gh auth login'"
    }
    has lavish-axi || {
        found_all=0
        missing lavish-axi optional "review pages" \
            "'npm install -g lavish-axi' (needs Node.js 22 or newer: https://nodejs.org/en/download)"
    }
    [ "$found_all" = 0 ] || say "  all found."
}

report_path() {
    # A PATH entry may end with a slash; the shell then finds dir//archon.
    case ":${PATH:-}:" in
    *":$INSTALL_DIR:"* | *":$INSTALL_DIR/:"*) say "$INSTALL_DIR is on your PATH." ;;
    *)
        say "$INSTALL_DIR is not on your PATH. Add this line to your shell profile:"
        say "  export PATH=\"$INSTALL_DIR:\$PATH\""
        return 0
        ;;
    esac
    first=$(command -v archon || true)
    case $first in
    "$INSTALL_DIR/archon" | "$INSTALL_DIR//archon") ;;
    *) warn "'archon' runs $first, which comes before $INSTALL_DIR on your PATH; remove it or reorder PATH." ;;
    esac
}

main() {
    target=$(detect_target)
    version=$(requested_version)
    INSTALL_DIR=$(resolve_install_dir)
    AETHER_INSTALL_URL=${ARCHON_AETHER_INSTALL_URL:-https://raw.githubusercontent.com/$AETHER_REPOSITORY/main/install.sh}
    choose_downloader
    require_sha256_tool
    has tar || die "tar was not found; install it and run this again"

    mkdir -p "$INSTALL_DIR" || die "could not create $INSTALL_DIR"
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' HUP TERM
    # Staged beside the destination: the rename is atomic and /tmp may be noexec.
    STAGING=$(mktemp -d "$INSTALL_DIR/.archon-install.XXXXXX") ||
        die "could not write to $INSTALL_DIR; set ARCHON_INSTALL_DIR to a directory you own"

    previous=$(installed_version)
    download_release "$target" "$version"
    unpack_release "$target"
    install_binaries
    report_install "$previous"

    aether_step
    report_prerequisites
    report_path
    [ "$AETHER_STEP_FAILED" = 1 ] || remote_step
    say "Next: archon setup"
    [ "$AETHER_STEP_FAILED" = 0 ] || exit "$EXIT_AETHER_FAILED"
}

main "$@"
