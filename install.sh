#!/usr/bin/env bash
# Sift (sift-llm) Installer
# Installs pre-compiled Sift binary for macOS and Linux.
# Usage: curl -fsSL https://raw.githubusercontent.com/arturoaguileraa/sift-llm/install.sh | bash

set -euo pipefail

REPO="arturoaguileraa/sift-llm"
BINARY_NAME="sift"

# Colors for terminal output
BOLD="\033[1m"
GREEN="\033[0;32m"
BLUE="\033[0;34m"
YELLOW="\033[0;33m"
RED="\033[0;31m"
RESET="\033[0m"

log_info() {
  printf "${BLUE}==>${RESET} ${BOLD}%s${RESET}\n" "$1"
}

log_success() {
  printf "${GREEN}✓${RESET} ${BOLD}%s${RESET}\n" "$1"
}

log_warning() {
  printf "${YELLOW}!${RESET} %s\n" "$1"
}

log_error() {
  printf "${RED}x${RESET} ${BOLD}%s${RESET}\n" "$1" >&2
}

# Require curl, tar, gzip
for cmd in curl tar gzip; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    log_error "Missing required command: $cmd"
    exit 1
  fi
done

# Detect OS
OS="$(uname -s)"
case "$OS" in
  Darwin)
    TARGET_OS="apple-darwin"
    ;;
  Linux)
    TARGET_OS="unknown-linux-gnu"
    ;;
  *)
    log_error "Unsupported operating system: $OS"
    exit 1
    ;;
esac

# Detect Architecture
ARCH="$(uname -m)"
case "$ARCH" in
  x86_64|amd64)
    TARGET_ARCH="x86_64"
    ;;
  arm64|aarch64)
    TARGET_ARCH="aarch64"
    ;;
  *)
    log_error "Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"

# Determine installation directory
if [ -n "${SIFT_INSTALL_DIR:-}" ]; then
  INSTALL_DIR="$SIFT_INSTALL_DIR"
elif [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
else
  INSTALL_DIR="$HOME/.sift/bin"
fi

# Determine version to install
if [ -n "${SIFT_VERSION:-}" ]; then
  VERSION="$SIFT_VERSION"
  case "$VERSION" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
  esac
else
  log_info "Fetching latest release version from GitHub..."
  LATEST_URL="https://api.github.com/repos/${REPO}/releases/latest"
  RELEASE_JSON="$(curl -fsSL -H "Accept: application/vnd.github+json" "$LATEST_URL" 2>/dev/null || true)"

  if [ -n "$RELEASE_JSON" ]; then
    VERSION="$(echo "$RELEASE_JSON" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')"
  fi

  if [ -z "${VERSION:-}" ]; then
    log_warning "Could not auto-detect latest tag from GitHub API. Falling back to latest release URL redirect."
    VERSION="latest"
  fi
fi

log_info "Installing Sift (${VERSION}) for ${TARGET}..."

# Setup temporary directory for download
TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'sift-install')"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

# Construct download URL
if [ "$VERSION" = "latest" ]; then
  DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/sift-${TARGET}.tar.gz"
else
  DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/sift-${VERSION}-${TARGET}.tar.gz"
fi

TARBALL="${TMP_DIR}/sift.tar.gz"

log_info "Downloading ${DOWNLOAD_URL}..."
if ! curl -fsSL "$DOWNLOAD_URL" -o "$TARBALL"; then
  # Try fallback naming without version tag in filename
  FALLBACK_URL="https://github.com/${REPO}/releases/download/${VERSION}/sift-${TARGET}.tar.gz"
  if ! curl -fsSL "$FALLBACK_URL" -o "$TARBALL"; then
    log_error "Failed to download binary release from GitHub."
    log_error "Please check that release ${VERSION} exists at https://github.com/${REPO}/releases"
    exit 1
  fi
fi

# Extract and install
tar -xzf "$TARBALL" -C "$TMP_DIR"

if [ ! -f "${TMP_DIR}/${BINARY_NAME}" ]; then
  log_error "Binary '${BINARY_NAME}' not found in downloaded package."
  exit 1
fi

mkdir -p "$INSTALL_DIR"
mv "${TMP_DIR}/${BINARY_NAME}" "${INSTALL_DIR}/${BINARY_NAME}"
chmod +x "${INSTALL_DIR}/${BINARY_NAME}"

log_success "Sift installed successfully to ${INSTALL_DIR}/${BINARY_NAME}"

# Ensure INSTALL_DIR is on PATH, managed as a delimited block so `sift uninstall`
# can remove it cleanly later.
SIFT_PATH_START="# >>> sift-llm >>>"
SIFT_PATH_END="# <<< sift-llm <<<"

add_to_path() {
  local rc
  case "$(basename "${SHELL:-}")" in
    zsh)  rc="$HOME/.zshrc" ;;
    bash) if [ "$OS" = "Darwin" ]; then rc="$HOME/.bash_profile"; else rc="$HOME/.bashrc"; fi ;;
    *)    rc="$HOME/.profile" ;;
  esac

  if [ -f "$rc" ] && grep -qF "$SIFT_PATH_START" "$rc"; then
    log_info "PATH entry already present in ${rc}."
    return
  fi

  {
    printf '\n%s\n' "$SIFT_PATH_START"
    printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    printf '%s\n' "$SIFT_PATH_END"
  } >> "$rc"

  log_success "Added ${INSTALL_DIR} to PATH in ${rc}."
  log_warning "Restart your shell or run: source ${rc}"
}

case ":$PATH:" in
  *:"$INSTALL_DIR":*) ;;  # already on PATH, nothing to do
  *) add_to_path ;;
esac

log_info "Run '${BINARY_NAME} --help' or '${BINARY_NAME} serve' to get started!"
