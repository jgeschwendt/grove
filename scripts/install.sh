#!/bin/bash
set -euo pipefail

# grove installer
# Usage: curl -fsSL https://jgeschwendt.github.io/grove/scripts/install.sh | bash
#
# Installs to ~/.grove/bin by default (no sudo required)
# Set GROVE_INSTALL_DIR to override

REPO="jgeschwendt/grove"
GROVE_HOME="${GROVE_HOME:-$HOME/.grove}"
INSTALL_DIR="${GROVE_INSTALL_DIR:-$GROVE_HOME/bin}"
VERSION="${1:-latest}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

info() { echo -e "${GREEN}info${NC}: $1"; }
warn() { echo -e "${YELLOW}warn${NC}: $1"; }
error() { echo -e "${RED}error${NC}: $1"; exit 1; }

# Download with curl or wget fallback
download() {
    local url="$1" dest="$2"
    if command -v curl &>/dev/null; then
        curl -fsSL "$url" -o "$dest"
    elif command -v wget &>/dev/null; then
        wget -qO "$dest" "$url"
    else
        error "Neither curl nor wget found. Install one and retry."
    fi
}

# Cross-platform SHA256 verification
sha256_verify() {
    local file="$1" expected="$2"
    local actual
    if command -v sha256sum &>/dev/null; then
        actual=$(sha256sum "$file" | cut -d' ' -f1)
    elif command -v shasum &>/dev/null; then
        actual=$(shasum -a 256 "$file" | cut -d' ' -f1)
    else
        warn "No sha256sum or shasum found, skipping verification"
        return 0
    fi
    if [[ "$actual" != "$expected" ]]; then
        error "Checksum mismatch!\n  Expected: ${expected}\n  Actual:   ${actual}"
    fi
}

# Detect OS
case "$(uname -s)" in
    Darwin) OS="darwin" ;;
    Linux)  OS="linux" ;;
    *)      error "Unsupported OS: $(uname -s)" ;;
esac

# Detect architecture (normalize to Rust target naming)
case "$(uname -m)" in
    x86_64)         ARCH="x86_64" ;;
    arm64|aarch64)  ARCH="aarch64" ;;
    *)              error "Unsupported architecture: $(uname -m)" ;;
esac

NAME="${OS}-${ARCH}"
info "Detected platform: ${NAME}"

# Get version (use redirect to avoid API rate limits)
if [[ "$VERSION" == "latest" ]]; then
    info "Fetching latest version..."
    if command -v curl &>/dev/null; then
        VERSION=$(curl -fsSI "https://github.com/${REPO}/releases/latest" | grep -i '^location:' | sed -E 's|.*/tag/([^[:space:]]+).*|\1|')
    else
        VERSION=$(wget --spider -S "https://github.com/${REPO}/releases/latest" 2>&1 | grep -i 'location:' | tail -1 | sed -E 's|.*/tag/([^[:space:]]+).*|\1|')
    fi
    if [[ -z "$VERSION" ]]; then
        error "Failed to fetch latest version"
    fi
fi

info "Installing grove ${VERSION}..."

# Check for existing installation
if [[ -x "${INSTALL_DIR}/grove" ]]; then
    EXISTING=$("${INSTALL_DIR}/grove" --version 2>/dev/null | head -1 || echo "unknown")
    info "Upgrading from ${EXISTING}"
fi

# Setup temp directory
TMP_DIR=$(mktemp -d)
trap "rm -rf $TMP_DIR" EXIT

# Download checksums
CHECKSUMS_URL="https://github.com/${REPO}/releases/download/${VERSION}/checksums.txt"
info "Fetching checksums..."
if ! download "$CHECKSUMS_URL" "${TMP_DIR}/checksums.txt"; then
    error "Failed to download checksums. Check that version ${VERSION} exists."
fi

# Download binary
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${VERSION}/${NAME}.tar.gz"
info "Downloading ${NAME}.tar.gz..."
if ! download "$DOWNLOAD_URL" "${TMP_DIR}/grove.tar.gz"; then
    error "Download failed. Check that version ${VERSION} has binaries for ${NAME}."
fi

# Verify checksum
EXPECTED_SHA=$(grep "${NAME}.tar.gz" "${TMP_DIR}/checksums.txt" | cut -d' ' -f1)
if [[ -z "$EXPECTED_SHA" ]]; then
    error "No checksum found for ${NAME}.tar.gz in checksums.txt"
fi
info "Verifying checksum..."
sha256_verify "${TMP_DIR}/grove.tar.gz" "$EXPECTED_SHA"

# Extract
tar -xzf "${TMP_DIR}/grove.tar.gz" -C "$TMP_DIR"

# Install binary
mkdir -p "$INSTALL_DIR"
mv "${TMP_DIR}/grove" "${INSTALL_DIR}/grove"
chmod +x "${INSTALL_DIR}/grove"

info "Installed grove to ${INSTALL_DIR}/grove"

# Verify installation works
if ! "${INSTALL_DIR}/grove" --version &>/dev/null; then
    error "Installation verification failed. Binary may be incompatible with this system."
fi

# Install man page (easter egg: man grove)
MAN_DIR="${GROVE_HOME}/share/man/man1"
if [[ -f "${TMP_DIR}/grove.1" ]]; then
    mkdir -p "$MAN_DIR"
    mv "${TMP_DIR}/grove.1" "${MAN_DIR}/grove.1"
    info "Installed man page to ${MAN_DIR}/grove.1"
fi

echo ""

# Check if in PATH
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    warn "$INSTALL_DIR is not in your PATH"
    echo ""
    echo "Add this to your shell profile (~/.zshrc or ~/.bashrc):"
    echo ""
    echo "  export PATH=\"\$HOME/.grove/bin:\$PATH\""
    echo "  export MANPATH=\"\$HOME/.grove/share/man:\$MANPATH\""
    echo ""
fi

echo "Run 'grove --help' to get started"
