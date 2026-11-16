#!/usr/bin/env bash
# Job-local fallback for Linux runners without Go. Never installs system packages.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "$#" -eq 0 ]; then
    echo "usage: bash scripts/go-toolchain.sh <command> [args...]" >&2
    exit 1
fi
if command -v go >/dev/null 2>&1 && [ "$(GOTOOLCHAIN=local go version | cut -d ' ' -f 3)" = go1.26.4 ]; then
    exec "$@"
fi
if [ "$(uname -s)" != Linux ]; then
    echo "Install Go 1.26.4; automatic bootstrap is Linux-only" >&2
    exit 1
fi
# SHA-256 values published by https://go.dev/dl/?mode=json&include=all.
case "$(uname -m)" in
    x86_64) ARCH=amd64; SHA=1153d3d50e0ac764b447adfe05c2bcf08e889d42a02e0fe0259bd47f6733ad7f ;;
    aarch64|arm64) ARCH=arm64; SHA=ef758ae7c6cf9267c9c0ef080b8965f453d89ab2d25d9eb22de4405925238768 ;;
    *) echo "Unsupported Linux host architecture" >&2; exit 1 ;;
esac
DIR="$ROOT/target/go-toolchain/linux-$ARCH"
FILE="go1.26.4.linux-$ARCH.tar.gz"
mkdir -p "$DIR"
if [ ! -f "$DIR/$FILE" ]; then
    curl -fsSL "https://go.dev/dl/$FILE" -o "$DIR/$FILE.download"
    mv "$DIR/$FILE.download" "$DIR/$FILE"
fi
printf '%s  %s\n' "$SHA" "$DIR/$FILE" | sha256sum -c -
if [ ! -x "$DIR/go/bin/go" ]; then
    tar -xzf "$DIR/$FILE" -C "$DIR"
fi
export PATH="$DIR/go/bin:$PATH"
exec "$@"
