#!/bin/sh
set -eu

REPO="chipturner/gritty"

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS-$ARCH" in
    Linux-x86_64)  TARGET=x86_64-unknown-linux-gnu ;;
    Linux-aarch64) TARGET=aarch64-unknown-linux-gnu ;;
    Darwin-x86_64) TARGET=x86_64-apple-darwin ;;
    Darwin-arm64)  TARGET=aarch64-apple-darwin ;;
    *) echo "error: unsupported platform: $OS $ARCH" >&2; exit 1 ;;
esac

VERSION="${1:-latest}"
if [ "$VERSION" = "latest" ]; then
    BASE="https://github.com/$REPO/releases/latest/download"
else
    BASE="https://github.com/$REPO/releases/download/v$VERSION"
fi

TARBALL="gritty-$TARGET.tar.gz"
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "downloading gritty ($TARGET)..."
curl -sSfL "$BASE/$TARBALL"   -o "$TMPDIR/$TARBALL"
curl -sSfL "$BASE/SHA256SUMS" -o "$TMPDIR/SHA256SUMS"

echo "verifying checksum..."
cd "$TMPDIR"
grep "$TARBALL" SHA256SUMS | sha256sum -c -

tar xzf "$TARBALL"

INSTALL_DIR="${GRITTY_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$INSTALL_DIR"
mv gritty "$INSTALL_DIR/gritty"

echo "installed gritty to $INSTALL_DIR/gritty"
case ":$PATH:" in
    *:"$INSTALL_DIR":*) ;;
    *) echo "note: add $INSTALL_DIR to your PATH" ;;
esac
