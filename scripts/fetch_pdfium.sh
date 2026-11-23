#!/usr/bin/env bash
set -euo pipefail

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)  PLATFORM="linux" ;;
  Darwin) PLATFORM="mac" ;;
  *)
    echo "Unsupported OS: $OS (download manually from https://github.com/bblanchon/pdfium-binaries/releases)"
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  PDF_ARCH="x64" ;;
  arm64|aarch64) PDF_ARCH="arm64" ;;
  *)
    echo "Unsupported arch: $ARCH"
    exit 1
    ;;
esac

URL="https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-${PLATFORM}-${PDF_ARCH}.tgz"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

echo "Downloading $URL ..."
curl -fL "$URL" -o "$TMP_DIR/pdfium.tgz"

mkdir -p ./lib
tar -xzf "$TMP_DIR/pdfium.tgz" -C "$TMP_DIR"
cp "$TMP_DIR"/lib/libpdfium.* ./lib/

echo "Installed PDFium library into ./lib/:"
ls -la ./lib/
