#!/usr/bin/env bash
# dgxtop installer.
#
#   curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash
#   ./install.sh --version v2.0.0
#   ./install.sh --rollback
#
# Hosts: Linux on x86_64 or aarch64 with glibc, WSL 2 included. Other systems,
# WSL 1 and musl are refused before anything is downloaded, and so are the
# releases before v2.0.0 (the earlier dgxtop, packaged differently).
#
# Safety properties:
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
FIRST_RELEASE="v2.0.0"
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

is_musl() {
    [ -f /etc/alpine-release ] && return 0
    # musl's `ldd --version` prints its banner on stderr and exits 1, so the
    # output is captured whatever the exit status.
    local report
    report="$(ldd --version 2>&1 || true)"
    case "$report" in
        *[Mm][Uu][Ss][Ll]*) return 0 ;;
    esac
    return 1
}

detect_target() {
    local system arch
    system="$(uname -s)"
    [ "$system" = "Linux" ] || fail "dgxtop supports Linux only (found ${system})"
    case "$(uname -r)" in
        # WSL 1 translates system calls without a Linux kernel: no pidfd, no GPU.
        *-Microsoft) fail "WSL 1 is not supported (kernel $(uname -r)); use WSL 2" ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64) arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) fail "unsupported architecture: $(uname -m)" ;;
    esac
    if is_musl; then
        fail "musl libc detected: only GNU builds are published (no musl fallback)"
    fi
    echo "${arch}-unknown-linux-gnu"
}

note_wsl2() {
    case "$(uname -r)" in
        *[Mm]icrosoft*) info "WSL 2 detected: GPUs need the NVIDIA driver installed on Windows; CPU temperature and power are usually not available" ;;
    esac
}

resolve_version() {
    local tag
    if [ -n "$VERSION" ]; then
        case "$VERSION" in
            v*) tag="$VERSION" ;;
            *) tag="v${VERSION}" ;;
        esac
    else
        tag="$(curl -fsSL --proto '=https' --tlsv1.2 "https://api.github.com/repos/${REPO}/releases/latest" \
            | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n 1)"
        [ -n "$tag" ] || fail "could not determine the latest release; pass --version"
    fi
    case "$tag" in
        v0.*|v1.*) fail "${tag} is a release of the earlier dgxtop, packaged differently; install ${FIRST_RELEASE} or later" ;;
    esac
    echo "$tag"
}

fetch() {
    local url="$1" destination="$2"
    case "$url" in
        file://*) cp -- "${url#file://}" "$destination" 2>/dev/null ;;
        https://*) curl -fsSL --proto '=https' --tlsv1.2 -o "$destination" "$url" ;;
        *) fail "refusing non-https URL: $url" ;;
    esac
}

host_glibc() {
    getconf GNU_LIBC_VERSION 2>/dev/null || echo "an unknown glibc"
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
    if ! report="$("$candidate" --version --verbose 2>&1)"; then
        case "$report" in
            *GLIBC_*) fail "new binary needs a newer glibc than this host has ($(host_glibc)): $report" ;;
        esac
        fail "new binary does not run on this host: $report"
    fi
    # Whole-line fixed strings: v0.2 must not accept 0.2.0, nor '.' any byte.
    # A here-string, not a pipe: under pipefail, grep -q leaving at the first
    # match would end the writer with SIGPIPE and fail a report that matched.
    grep -qxF -- "dgxtop ${version#v}" <<<"$report" || fail "binary version does not match ${version}"
    grep -qxF -- "target: ${target}" <<<"$report" || fail "binary target does not match ${target}"
}

install_release() {
    local target version base archive_name
    # The host is checked first, so an unsupported system hears that rather
    # than which of the tools below it lacks.
    target="$(detect_target)"
    note_wsl2
    require curl
    require tar
    require sha256sum
    version="$(resolve_version)"
    base="${BASE_URL:-https://github.com/${REPO}/releases/download/${version}}"
    archive_name="${BINARY}-${version}-${target}.tar.gz"

    mkdir -p "$INSTALL_DIR"
    STAGING="$(mktemp -d "${INSTALL_DIR}/.${BINARY}-install.XXXXXX")"
    local staging="$STAGING"

    info "downloading ${archive_name}"
    fetch "${base}/${archive_name}" "${staging}/${archive_name}" \
        || fail "could not download ${archive_name} from ${base}: release ${version} or its ${target} build does not exist"
    fetch "${base}/SHA256SUMS" "${staging}/SHA256SUMS" \
        || fail "could not download SHA256SUMS from ${base}"
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
