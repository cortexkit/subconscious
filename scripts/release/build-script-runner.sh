#!/usr/bin/env bash
# Embed the registration helper for the same target as the compiled Bun image.
set -euo pipefail
target="${1:?Rust target required}"
platform="${2:?Bun platform required}"
arch="${3:?architecture required}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
case "$platform" in
  linux)
    cargo zigbuild --locked --release -p subc-registration-native --target "$target.2.28"
    library=libsubc_registration_native.so
    ;;
  darwin)
    cargo build --locked --release -p subc-registration-native --target "$target"
    library=libsubc_registration_native.dylib
    identity="${DEVELOPER_ID_APPLICATION_IDENTITY:?signing identity required}"
    if [[ "$identity" == - ]]; then
      codesign --force --options runtime --sign "$identity" "target/$target/release/$library"
    else
      codesign --force --options runtime --timestamp --sign "$identity" "target/$target/release/$library"
    fi
    ;;
  win32)
    cargo build --locked --release -p subc-registration-native --target "$target"
    library=subc_registration_native.dll
    ;;
  *) echo "unsupported runner platform: $platform" >&2; exit 1 ;;
esac
mkdir -p clients/subc-client/native
cp "target/$target/release/$library" "clients/subc-client/native/subc-registration-$platform-$arch.node"
cd clients/script-runner
bun install --frozen-lockfile
bun run typecheck
export BUN_COMPILE_TARGET="bun-${platform/win32/windows}-$arch"
bun run build
suffix=""
if [[ "$platform" == win32 ]]; then suffix=.exe; fi
cp "dist/ck-script-runner-bun$suffix" "$root/target/$target/release/ck-script-runner-bun$suffix"
