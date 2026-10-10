#!/usr/bin/env bash
# dgxtop installer.
#
#   curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
#   ./install.sh --version v0.2.0
#   ./install.sh --rollback
#
# Safety properties (spec/dgxtop-v2/OPERATIONS.md §4):
# * downloads into a temporary directory on the SAME filesystem as the target,
# * verifies the archive against SHA256SUMS and the expected target triple,
# * accepts an archive containing exactly one regular file named `dgxtop`
#   (no absolute paths, no `..`, no links),
# * runs `dgxtop --version --verbose` to prove the binary loads on this host,
# * replaces the installed binary with an atomic rename and keeps the previous
#   binary as `dgxtop.previous` for rollback,
# * never falls back from a GNU to a musl build.
# Rollback restores the previous binary only: configuration and state stay
# untouched, and signals already sent to processes cannot be rolled back.
set -euo pipefail

REPO="DennySORA/dgxtop"
BINARY="dgxtop"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"
VERSION=""
BASE_URL=""
ROLLBACK=0
STAGING=""

cleanup() {
    if [ -n "$STAGING" ] && [ -d "$STAGING" ]; then
        rm -rf -- "$STAGING"
    fi
}
trap cleanup EXIT

info() { printf '\033[1;34m[info]\033[0m %s\n' "$1"; }
warn() { printf '\033[1;33m[warn]\033[0m %s\n' "$1" >&2; }
fail() { printf '\033[1;31m[error]\033[0m %s\n' "$1" >&2; exit 1; }

usage() {
    cat <<'EOF'
Usage: install.sh [--version vX.Y.Z] [--install-dir DIR] [--base-url URL] [--rollback]

  --version      release tag to install (default: latest release)
  --install-dir  target directory (default: $INSTALL_DIR or ~/.local/bin)
  --base-url     directory URL holding the release assets (default: GitHub release)
  --rollback     restore the previously installed binary (dgxtop.previous)
EOF
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
            --install-dir) INSTALL_DIR="${2:?--install-dir needs a value}"; shift 2 ;;
            --base-url) BASE_URL="${2:?--base-url needs a value}"; shift 2 ;;
            --rollback) ROLLBACK=1; shift ;;
            -h|--help) usage; exit 0 ;;
            *) fail "unknown argument: $1" ;;
        esac
    done
}

require() {
    command -v "$1" >/dev/null 2>&1 || fail "required tool not found: $1"
}

detect_target() {
    [ "$(uname -s)" = "Linux" ] || fail "dgxtop supports Linux only (found $(uname -s))"
    local arch
    case "$(uname -m)" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) fail "unsupported architecture: $(uname -m)" ;;
    esac
    if ldd --version 2>&1 | grep -qi musl || [ -f /etc/alpine-release ]; then
        fail "musl libc detected: only GNU builds are published (no musl fallback)"
    fi
    echo "${arch}-unknown-linux-gnu"
}

resolve_version() {
    if [ -n "$VERSION" ]; then
        echo "$VERSION"
        return
    fi
    local tag
    tag="$(curl -fsSL --proto '=https' --tlsv1.2 "https://api.github.com/repos/${REPO}/releases/latest" \
        | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$tag" ] || fail "could not determine the latest release; pass --version"
    echo "$tag"
}

fetch() {
    local url="$1" destination="$2"
    case "$url" in
        file://*) cp -- "${url#file://}" "$destination" ;;
        https://*) curl -fsSL --proto '=https' --tlsv1.2 -o "$destination" "$url" ;;
        *) fail "refusing non-https URL: $url" ;;
    esac
}

verify_checksum() {
    local archive="$1" sums="$2" name expected actual
    name="$(basename "$archive")"
    expected="$(awk -v file="$name" '$2 == file || $2 == "*" file { print $1 }' "$sums" | head -n 1)"
    [ -n "$expected" ] || fail "SHA256SUMS has no entry for $name"
    actual="$(sha256sum "$archive" | awk '{ print $1 }')"
    [ "$expected" = "$actual" ] || fail "checksum mismatch for $name (expected $expected, got $actual)"
}

verify_archive_layout() {
    local archive="$1" listing entries
    listing="$(tar -tzvf "$archive")" || fail "archive is unreadable"
    entries="$(printf '%s\n' "$listing" | grep -c . || true)"
    [ "$entries" = "1" ] || fail "archive must contain exactly one entry (found $entries)"
    case "$(printf '%s' "$listing" | cut -c1)" in
        -) ;;
        *) fail "archive entry is not a regular file" ;;
    esac
    local entry
    entry="$(tar -tzf "$archive")"
    case "$entry" in
        "$BINARY"|"./$BINARY") ;;
        *) fail "unexpected archive entry: $entry" ;;
    esac
}

verify_binary() {
    local candidate="$1" version="$2" target="$3" report
    report="$("$candidate" --version --verbose 2>&1)" || fail "new binary does not run on this host: $report"
    # Whole-line fixed strings: v0.2 must not accept 0.2.0, nor '.' any byte.
    printf '%s\n' "$report" | grep -qxF "dgxtop ${version#v}" || fail "binary version does not match ${version}"
    printf '%s\n' "$report" | grep -qxF "target: ${target}" || fail "binary target does not match ${target}"
}

install_release() {
    require curl
    require tar
    require sha256sum
    local target version base archive_name
    target="$(detect_target)"
    version="$(resolve_version)"
    base="${BASE_URL:-https://github.com/${REPO}/releases/download/${version}}"
    archive_name="${BINARY}-${version}-${target}.tar.gz"

    mkdir -p "$INSTALL_DIR"
    STAGING="$(mktemp -d "${INSTALL_DIR}/.${BINARY}-install.XXXXXX")"
    local staging="$STAGING"

    info "downloading ${archive_name}"
    fetch "${base}/${archive_name}" "${staging}/${archive_name}"
    fetch "${base}/SHA256SUMS" "${staging}/SHA256SUMS"
    verify_checksum "${staging}/${archive_name}" "${staging}/SHA256SUMS"
    verify_archive_layout "${staging}/${archive_name}"

    mkdir "${staging}/extract"
    tar -xzf "${staging}/${archive_name}" -C "${staging}/extract" --no-same-owner --no-same-permissions
    local candidate="${staging}/extract/${BINARY}"
    [ -f "$candidate" ] && [ ! -L "$candidate" ] || fail "extracted binary missing or not a regular file"
    chmod 0755 "$candidate"
    verify_binary "$candidate" "$version" "$target"

    local destination="${INSTALL_DIR}/${BINARY}"
    if [ -f "$destination" ]; then
        cp -p -- "$destination" "${staging}/${BINARY}.previous"
        mv -f -- "${staging}/${BINARY}.previous" "${destination}.previous"
        info "previous binary kept as ${destination}.previous"
    fi
    mv -f -- "$candidate" "$destination"
    info "installed ${BINARY} ${version} (${target}) to ${destination}"

    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *) warn "${INSTALL_DIR} is not in PATH" ;;
    esac
}

rollback() {
    local destination="${INSTALL_DIR}/${BINARY}" previous="${INSTALL_DIR}/${BINARY}.previous"
    [ -f "$previous" ] || fail "no previous binary at ${previous}"
    "$previous" --version >/dev/null 2>&1 || warn "previous binary did not report a version"
    STAGING="$(mktemp -d "${INSTALL_DIR}/.${BINARY}-rollback.XXXXXX")"
    local staging="$STAGING"
    if [ -f "$destination" ]; then
        cp -p -- "$destination" "${staging}/${BINARY}.rolled-back"
    fi
    mv -f -- "$previous" "$destination"
    if [ -f "${staging}/${BINARY}.rolled-back" ]; then
        mv -f -- "${staging}/${BINARY}.rolled-back" "$previous"
    fi
    info "restored the previous binary; configuration and state were not modified"
    info "signals already sent to processes cannot be rolled back"
}

main() {
    parse_args "$@"
    if [ "$ROLLBACK" = "1" ]; then
        rollback
    else
        install_release
    fi
}

main "$@"
