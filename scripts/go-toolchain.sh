#!/usr/bin/env bash
# Job-local pinned Go bootstrap. Never installs system packages.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
if [ "$#" -eq 0 ]; then
    echo "usage: bash scripts/go-toolchain.sh <command> [args...]" >&2
    exit 1
fi
if command -v go >/dev/null 2>&1 && [ "$(GOTOOLCHAIN=local go version | cut -d ' ' -f 3)" = go1.26.4 ]; then
    exec "$@"
fi
# SHA-256 values published by https://go.dev/dl/?mode=json&include=all.
case "$(uname -s):$(uname -m)" in
    Linux:x86_64) HOST=linux-amd64; SHA=1153d3d50e0ac764b447adfe05c2bcf08e889d42a02e0fe0259bd47f6733ad7f ;;
    Linux:aarch64|Linux:arm64) HOST=linux-arm64; SHA=ef758ae7c6cf9267c9c0ef080b8965f453d89ab2d25d9eb22de4405925238768 ;;
    Darwin:arm64) HOST=darwin-arm64; SHA=b62ad2b6d7d2464f12a5bcad7ff47f19d08325773b5efd21610e445a05a9bf53 ;;
    MINGW*:x86_64|MSYS*:x86_64) HOST=windows-amd64; SHA=3ca8fb4630b07c419cbdd51f754e31363cfcfb83b3a5354d9e895c90be2cc345 ;;
    *) echo "Unsupported Go bootstrap host" >&2; exit 1 ;;
esac
EXT=tar.gz
if [ "$HOST" = windows-amd64 ]; then EXT=zip; fi
DIR="$ROOT/target/go-toolchain/$HOST"
FILE="go1.26.4.$HOST.$EXT"
mkdir -p "$DIR"
if [ ! -f "$DIR/$FILE" ]; then
    curl -fsSL "https://go.dev/dl/$FILE" -o "$DIR/$FILE.download"
    mv "$DIR/$FILE.download" "$DIR/$FILE"
fi
if command -v sha256sum >/dev/null; then
    printf '%s  %s\n' "$SHA" "$DIR/$FILE" | sha256sum -c -
else
    printf '%s  %s\n' "$SHA" "$DIR/$FILE" | shasum -a 256 -c -
fi
if [ ! -x "$DIR/go/bin/go" ] && [ ! -x "$DIR/go/bin/go.exe" ]; then
    if [ "$EXT" = zip ]; then
        unzip -q "$DIR/$FILE" -d "$DIR"
    else
        tar -xzf "$DIR/$FILE" -C "$DIR"
    fi
fi
export PATH="$DIR/go/bin:$PATH"
exec "$@"
