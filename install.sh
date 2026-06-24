#!/bin/sh
# Phoenix installer — detects platform and downloads the latest release.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rmsap/phoenixlang/main/install.sh | sudo sh

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

# Standalone `phoenix-gen` (codegen-only entry point; same implementation as
# `phoenix gen`). Guarded so installing against an older release that predates
# it still succeeds.
if [ -f "$TMP/phoenix-gen" ]; then
    cp "$TMP/phoenix-gen" "$INSTALL_DIR/phoenix-gen"
    chmod +x "$INSTALL_DIR/phoenix-gen"
    GEN_INSTALLED=true
fi

# Install runtime library (needed by `phoenix build`)
LIB_DIR="$(dirname "$INSTALL_DIR")/lib"
mkdir -p "$LIB_DIR"
if [ -f "$TMP/lib/libphoenix_runtime.a" ]; then
    cp "$TMP/lib/libphoenix_runtime.a" "$LIB_DIR/libphoenix_runtime.a"
    RUNTIME_INSTALLED=true
fi

# Install the wasm runtime (needed by `phoenix build --target wasm32-linear`).
# Discovery finds it via the `bin/../lib/` install layout, same as the static
# lib above.
if [ -f "$TMP/lib/phoenix_runtime.wasm" ]; then
    cp "$TMP/lib/phoenix_runtime.wasm" "$LIB_DIR/phoenix_runtime.wasm"
    WASM_RUNTIME_INSTALLED=true
fi

# Clean up
rm -rf "$TMP"

if [ "$GEN_INSTALLED" = true ]; then
    echo "Installed phoenix, phoenix-lsp, and phoenix-gen to $INSTALL_DIR"
else
    echo "Installed phoenix and phoenix-lsp to $INSTALL_DIR"
fi
if [ "$RUNTIME_INSTALLED" = true ]; then
    echo "Installed libphoenix_runtime.a to $LIB_DIR"
else
    echo "Warning: libphoenix_runtime.a not found in release archive (phoenix build will not work)"
fi
if [ "$WASM_RUNTIME_INSTALLED" = true ]; then
    echo "Installed phoenix_runtime.wasm to $LIB_DIR"
else
    echo "Warning: phoenix_runtime.wasm not found in release archive (phoenix build --target wasm32-linear will not work)"
fi
echo ""
echo "Make sure $INSTALL_DIR is in your PATH, then run:"
echo "  phoenix --version"
