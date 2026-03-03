#!/usr/bin/env bash
set -euo pipefail

# DGXTop installer
# Usage: curl -fsSL https://raw.githubusercontent.com/DennySORA/dgxtop/main/install.sh | bash

REPO="DennySORA/dgxtop"
BINARY="dgxtop"
INSTALL_DIR="${INSTALL_DIR:-$HOME/.local/bin}"

info() { printf "\033[1;34m[info]\033[0m %s\n" "$1"; }
warn() { printf "\033[1;33m[warn]\033[0m %s\n" "$1"; }
error() { printf "\033[1;31m[error]\033[0m %s\n" "$1" >&2; exit 1; }

detect_os() {
    case "$(uname -s)" in
        Linux*)  echo "linux" ;;
        Darwin*) error "dgxtop is designed for Linux (NVIDIA DGX systems). macOS is not supported." ;;
        *)       error "Unsupported operating system: $(uname -s)" ;;
    esac
}

detect_arch() {
    case "$(uname -m)" in
        x86_64|amd64)   echo "x86_64" ;;
        aarch64|arm64)   echo "aarch64" ;;
        *)               error "Unsupported architecture: $(uname -m)" ;;
    esac
}

detect_libc() {
    # Prefer ldd signature when available.
    if command -v ldd &>/dev/null; then
        local ldd_out
        ldd_out="$(ldd --version 2>&1 || true)"
        if echo "$ldd_out" | grep -qi "musl"; then
            echo "musl"
            return
        fi
        if echo "$ldd_out" | grep -qiE "glibc|gnu libc"; then
            echo "gnu"
            return
        fi
    fi

    # Alpine is musl by default.
    if [ -f /etc/alpine-release ]; then
        echo "musl"
        return
    fi

    # Default to glibc on mainstream Linux distros.
    echo "gnu"
}

get_target() {
    if [ -n "${TARGET:-}" ]; then
        echo "$TARGET"
        return
    fi

    local os arch libc
    os=$(detect_os)
    arch=$(detect_arch)
    libc=$(detect_libc)

    echo "${arch}-unknown-${os}-${libc}"
}

fallback_target() {
    local target="$1"
    case "$target" in
        *-gnu)  echo "${target%-gnu}-musl" ;;
        *-musl) echo "${target%-musl}-gnu" ;;
        *)      echo "" ;;
    esac
}

download_tarball() {
    local version="$1" target="$2" dest="$3"
    local url="https://github.com/${REPO}/releases/download/${version}/${BINARY}-${target}.tar.gz"
    info "Downloading ${url}..."
    curl -fsSL "$url" -o "$dest"
}

get_latest_version() {
    if [ -n "${VERSION:-}" ]; then
        echo "$VERSION"
        return
    fi

    local version
    version=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
        | grep '"tag_name"' \
        | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')

    if [ -z "$version" ]; then
        error "Failed to fetch latest version from GitHub. Set VERSION env var to install a specific version."
    fi

    echo "$version"
}

main() {
    info "Installing ${BINARY}..."

    local target fallback version tmp_dir tarball
    target=$(get_target)
    version=$(get_latest_version)

    info "Version:  ${version}"
    info "Target:   ${target}"
    info "Location: ${INSTALL_DIR}"

    tmp_dir=$(mktemp -d)
    trap "rm -rf '$tmp_dir'" EXIT
    tarball="${tmp_dir}/${BINARY}.tar.gz"

    if ! download_tarball "$version" "$target" "$tarball"; then
        fallback=$(fallback_target "$target")
        if [ -n "$fallback" ]; then
            warn "No release artifact for '${target}'. Trying '${fallback}'..."
            download_tarball "$version" "$fallback" "$tarball" \
                || error "Download failed for both '${target}' and '${fallback}' on version '${version}'."
            target="$fallback"
        else
            error "Download failed. Check that version '${version}' exists and has a build for '${target}'."
        fi
    fi

    if [[ "$target" == *-musl ]]; then
        warn "Using musl build (${target})."
        warn "If GPU metrics are missing, install a glibc build instead (TARGET='${target%-musl}-gnu')."
    fi

    info "Extracting..."
    tar xzf "$tarball" -C "$tmp_dir"

    info "Installing to ${INSTALL_DIR}..."
    mkdir -p "$INSTALL_DIR"
    mv "${tmp_dir}/${BINARY}" "${INSTALL_DIR}/${BINARY}"
    chmod +x "${INSTALL_DIR}/${BINARY}"

    # Check PATH
    if ! echo "$PATH" | tr ':' '\n' | grep -qx "$INSTALL_DIR"; then
        warn "${INSTALL_DIR} is not in your PATH."
        warn "Add the following to your shell profile (~/.bashrc, ~/.zshrc, etc.):"
        warn "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    fi

    info "Successfully installed ${BINARY} ${version} to ${INSTALL_DIR}/${BINARY}"

    # Check for NVIDIA drivers
    if ! command -v nvidia-smi &>/dev/null; then
        warn "NVIDIA drivers not detected. GPU monitoring requires NVIDIA drivers with NVML."
        warn "Install NVIDIA drivers or run with --no-gpu for system-only monitoring."
    fi
}

main "$@"
