#!/usr/bin/env bash
# Link the pinned CPython bitcode against a real glibc 2.28 with Rust's LLVM linker.
set -euo pipefail
target="${1:?usage: $0 <linux-rust-target> [--test]}"
shift
case "$target" in
  x86_64-unknown-linux-gnu) image=quay.io/pypa/manylinux_2_28_x86_64 ;;
  aarch64-unknown-linux-gnu) image=quay.io/pypa/manylinux_2_28_aarch64 ;;
  *) echo "unsupported Python Linux release target: $target" >&2; exit 2 ;;
esac
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# Match the host's selected Rust toolchain, including its LLVM bitcode reader.
rust_version="$(rustc --version | cut -d' ' -f2)"
# The container runs as root, so files it creates under target/ would be root-owned and later
# steps on the host (which write their own scratch folders under target/) would be refused.
# Hand target/ back to the calling user however the build ends.
docker run --rm -v "$root:/io" -w /io \
  -e RUST_VERSION="$rust_version" -e RUST_TARGET="$target" \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  "$image" bash -ec '
    trap '\''chown -R "$HOST_UID:$HOST_GID" /io/target 2>/dev/null || true'\'' EXIT
    dnf install -y zstd
    export RUSTUP_HOME=/root/python-rustup CARGO_HOME=/root/python-cargo
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION"
    export PATH="$CARGO_HOME/bin:$PATH"
    bash scripts/release/build-script-runner-python.sh "$RUST_TARGET" "$@"
  ' bash "$@"
