#!/bin/sh
# Phoenix installer — detects platform and downloads the latest release.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | sh

set -e

REPO="rmsap/phoenixlang"
INSTALL_DIR="${PHOENIX_INSTALL_DIR:-/usr/local/bin}"

# Detect platform
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Linux)  PLATFORM="x86_64-unknown-linux-gnu" ;;
    Darwin)
        case "$ARCH" in
            x86_64)  PLATFORM="x86_64-apple-darwin" ;;
            arm64)   PLATFORM="aarch64-apple-darwin" ;;
            *)       echo "error: unsupported architecture: $ARCH"; exit 1 ;;
        esac
        ;;
    *)      echo "error: unsupported OS: $OS (use Windows builds from GitHub Releases)"; exit 1 ;;
esac

# Get latest release tag
LATEST=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name"' | head -1 | cut -d'"' -f4)
if [ -z "$LATEST" ]; then
    echo "error: could not determine latest release"
    exit 1
fi

echo "Installing Phoenix $LATEST for $PLATFORM..."

# Download and extract
URL="https://github.com/$REPO/releases/download/$LATEST/phoenix-$LATEST-$PLATFORM.tar.gz"
TMP=$(mktemp -d)
curl -fsSL "$URL" -o "$TMP/phoenix.tar.gz"
tar xzf "$TMP/phoenix.tar.gz" -C "$TMP"

# Install binaries
mkdir -p "$INSTALL_DIR"
cp "$TMP/phoenix" "$INSTALL_DIR/phoenix"
cp "$TMP/phoenix-lsp" "$INSTALL_DIR/phoenix-lsp"
chmod +x "$INSTALL_DIR/phoenix" "$INSTALL_DIR/phoenix-lsp"

# Clean up
rm -rf "$TMP"

echo "Installed phoenix and phoenix-lsp to $INSTALL_DIR"
echo ""
echo "Make sure $INSTALL_DIR is in your PATH, then run:"
echo "  phoenix --version"
