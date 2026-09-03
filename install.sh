#!/bin/sh
# wsblast installer
# Installs the pre-compiled wsblast binary for macOS or Linux.
# Usage: curl -fsSL https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/install.sh | sh

set -e

REPO="jephter-olamiposi/wsblast"
BIN_NAME="wsblast"

# Detect Operating System
OS="$(uname -s)"
case "$OS" in
    Darwin)
        TARGET_OS="apple-darwin"
        ;;
    Linux)
        TARGET_OS="unknown-linux-gnu"
        ;;
    *)
        echo "Error: Unsupported operating system: $OS"
        echo "wsblast supports macOS (Darwin) and Linux."
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
        echo "Error: Unsupported architecture: $ARCH"
        exit 1
        ;;
esac

TARGET="${TARGET_ARCH}-${TARGET_OS}"
TARBALL="${BIN_NAME}-${TARGET}.tar.gz"

echo "Detected platform: ${TARGET}"

# Determine latest release version from GitHub API
RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"
LATEST_TAG="$(curl -fsSL "${RELEASE_URL}" 2>/dev/null | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' || true)"

if [ -z "$LATEST_TAG" ]; then
    LATEST_TAG="v0.1.0"
fi

DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${TARBALL}"

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'wsblast')"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

echo "Downloading wsblast ${LATEST_TAG} for ${TARGET}..."
if curl -fsSL -o "${TMP_DIR}/${TARBALL}" "$DOWNLOAD_URL"; then
    tar -xzf "${TMP_DIR}/${TARBALL}" -C "$TMP_DIR"
else
    echo "Pre-compiled release tarball not found at ${DOWNLOAD_URL}"
    echo "Falling back to installing via Cargo from source..."
    if command -v cargo >/dev/null 2>&1; then
        cargo install --git "https://github.com/${REPO}.git"
        echo "wsblast installed successfully via Cargo."
        exit 0
    else
        echo "Error: Cargo is not installed. Please build from source or install Rust."
        exit 1
    fi
fi

# Determine target installation directory
INSTALL_DIR="/usr/local/bin"
if [ ! -w "$INSTALL_DIR" ]; then
    if command -v sudo >/dev/null 2>&1; then
        echo "Elevating permissions with sudo to install to ${INSTALL_DIR}..."
        sudo mv "${TMP_DIR}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
        sudo chmod +x "${INSTALL_DIR}/${BIN_NAME}"
    else
        INSTALL_DIR="${HOME}/.local/bin"
        mkdir -p "$INSTALL_DIR"
        mv "${TMP_DIR}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
        chmod +x "${INSTALL_DIR}/${BIN_NAME}"
        echo "Note: Installed to ${INSTALL_DIR}. Ensure this directory is in your PATH."
    fi
else
    mv "${TMP_DIR}/${BIN_NAME}" "${INSTALL_DIR}/${BIN_NAME}"
    chmod +x "${INSTALL_DIR}/${BIN_NAME}"
fi

echo ""
echo "wsblast ${LATEST_TAG} installed successfully to ${INSTALL_DIR}/${BIN_NAME}!"
echo "Run 'wsblast --help' to get started."
