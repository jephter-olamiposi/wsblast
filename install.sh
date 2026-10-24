#!/bin/sh
# wsblast installer
# Installs the pre-compiled wsblast binary for macOS or Linux.
# Usage: curl -fsSL https://raw.githubusercontent.com/jephter-olamiposi/wsblast/main/install.sh | sh
# Set WSBLAST_VERSION (e.g. v0.1.1) to install a specific release without querying the GitHub API.

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

# Resolve the release tag: WSBLAST_VERSION pins one, otherwise ask the GitHub API.
if [ -n "${WSBLAST_VERSION:-}" ]; then
    LATEST_TAG="$WSBLAST_VERSION"
else
    RELEASE_URL="https://api.github.com/repos/${REPO}/releases/latest"
    if ! RELEASE_JSON="$(curl -fsSL "$RELEASE_URL")"; then
        echo "Error: could not query the latest wsblast release from ${RELEASE_URL}."
        echo "Check your network connection or GitHub API rate limit, or pin a version with:"
        echo "  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | WSBLAST_VERSION=v0.1.1 sh"
        exit 1
    fi
    LATEST_TAG="$(printf '%s\n' "$RELEASE_JSON" | grep '"tag_name":' | head -n 1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')"
    if [ -z "$LATEST_TAG" ]; then
        echo "Error: the GitHub API response from ${RELEASE_URL} did not contain a release tag."
        echo "Pin a version with:"
        echo "  curl -fsSL https://raw.githubusercontent.com/${REPO}/main/install.sh | WSBLAST_VERSION=v0.1.1 sh"
        exit 1
    fi
fi

DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${TARBALL}"

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'wsblast')"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

echo "Downloading wsblast ${LATEST_TAG} for ${TARGET}..."
if ! curl -fsSL -o "${TMP_DIR}/${TARBALL}" "$DOWNLOAD_URL"; then
    echo "Pre-compiled release tarball not found at ${DOWNLOAD_URL}"
    echo "Falling back to building ${LATEST_TAG} from source with Cargo..."
    if command -v cargo >/dev/null 2>&1; then
        cargo install --locked --git "https://github.com/${REPO}.git" --tag "$LATEST_TAG" "$BIN_NAME"
        echo "wsblast installed successfully via Cargo."
        exit 0
    else
        echo "Error: Cargo is not installed. Please build from source or install Rust."
        exit 1
    fi
fi

if ! curl -fsSL -o "${TMP_DIR}/${TARBALL}.sha256" "${DOWNLOAD_URL}.sha256"; then
    echo "Error: checksum file not found at ${DOWNLOAD_URL}.sha256; refusing to install."
    exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL_SHA="$(sha256sum "${TMP_DIR}/${TARBALL}" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL_SHA="$(shasum -a 256 "${TMP_DIR}/${TARBALL}" | awk '{print $1}')"
else
    echo "Error: neither sha256sum nor shasum is available to verify the download; refusing to install."
    exit 1
fi
EXPECTED_SHA="$(awk 'NR == 1 {print tolower($1)}' "${TMP_DIR}/${TARBALL}.sha256")"

if [ -z "$EXPECTED_SHA" ] || [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
    echo "Error: SHA-256 checksum mismatch for ${TARBALL}; refusing to install."
    echo "  expected: ${EXPECTED_SHA:-<empty>}"
    echo "  actual:   ${ACTUAL_SHA}"
    exit 1
fi
echo "Checksum verified (SHA-256 ${ACTUAL_SHA})."

tar -xzf "${TMP_DIR}/${TARBALL}" -C "$TMP_DIR"

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
