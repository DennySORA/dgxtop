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

get_target() {
    local os arch
    os=$(detect_os)
    arch=$(detect_arch)

    # Use musl for static linking (portable across Linux distros)
    echo "${arch}-unknown-${os}-musl"
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

    local target version url tmp_dir
    target=$(get_target)
    version=$(get_latest_version)

    info "Version:  ${version}"
    info "Target:   ${target}"
    info "Location: ${INSTALL_DIR}"

    url="https://github.com/${REPO}/releases/download/${version}/${BINARY}-${target}.tar.gz"

    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT

    info "Downloading ${url}..."
    curl -fsSL "$url" -o "${tmp_dir}/${BINARY}.tar.gz" \
        || error "Download failed. Check that version '${version}' exists and has a build for '${target}'."

    info "Extracting..."
    tar xzf "${tmp_dir}/${BINARY}.tar.gz" -C "$tmp_dir"

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
