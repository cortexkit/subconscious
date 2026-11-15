#!/usr/bin/env bash
# Build ck-script-runner-python for one Rust target from its pinned CPython archive.
#
#   build-script-runner-python.sh <rust-target> [--sign]
#
# The archive named for <rust-target> in clients/script-runner-python/python-archives.lock is
# downloaded once into target/python-dist/archives/, checked against the pinned size and SHA-256,
# and extracted into target/python-dist/<rust-target>/. Cargo's build script only reads that
# extracted copy (CK_PYTHON_DIST); it never downloads anything itself.
#
# The pure-Python standard library is compiled to bytecode by CK_PYTHON_HOST, a CPython 3.13 that
# runs on the build machine. For a native build it defaults to the archive's own interpreter;
# a cross build must name one.
#
# --sign (macOS only) signs the binary with the hardened runtime and no entitlements, checks the
# signature, and runs the signed copy. DEVELOPER_ID_APPLICATION_IDENTITY=- signs ad hoc, as CI
# does; otherwise the identity comes from macos-sign-notarize.sh's own resolution.
set -euo pipefail

target="${1:?usage: $0 <rust-target> [--sign]}"
sign="${2:-}"
if [[ -n "$sign" && "$sign" != --sign ]]; then
  echo "usage: $0 <rust-target> [--sign]" >&2
  exit 2
fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
crate="clients/script-runner-python"
lock="$crate/python-archives.lock"

fail() {
  echo "$*" >&2
  exit 1
}

size="" sha="" url=""
read -r size sha url < <(awk -v target="$target" '$1 == target { print $2, $3, $4 }' "$lock") || true
[[ -n "$url" ]] || fail "no CPython archive is pinned for $target in $lock"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

matches_pin() {
  [[ -f "$1" && "$(wc -c < "$1" | tr -d ' ')" == "$size" && "$(sha256 "$1")" == "$sha" ]]
}

dist="$root/target/python-dist"
archive="$dist/archives/$target.tar.zst"
mkdir -p "$dist/archives"
if ! matches_pin "$archive"; then
  rm -f "$archive.part"
  curl -fsSL --retry 3 -o "$archive.part" "$url"
  if ! matches_pin "$archive.part"; then
    rm -f "$archive.part"
    fail "refusing $url: its size or SHA-256 differs from $lock"
  fi
  mv "$archive.part" "$archive"
fi

# The stamp records which pinned archive the extracted tree came from, so a pin change
# re-extracts instead of building against stale files.
extracted="$dist/$target"
if [[ "$(cat "$extracted/.archive-sha256" 2>/dev/null || true)" != "$sha" ]]; then
  rm -rf "$extracted"
  mkdir -p "$extracted"
  zstd -dc "$archive" | tar -xf - -C "$extracted"
  echo "$sha" > "$extracted/.archive-sha256"
fi

host="$(rustc -vV | sed -n 's/^host: //p')"
if [[ -z "${CK_PYTHON_HOST:-}" ]]; then
  [[ "$host" == "$target" ]] || fail "cross build for $target on $host: set CK_PYTHON_HOST to a CPython 3.13 that runs here"
  CK_PYTHON_HOST="$extracted/python/install/bin/python3"
fi
export CK_PYTHON_DIST="$extracted/python"
export CK_PYTHON_HOST
if [[ "$target" == *-apple-darwin ]]; then
  # Build the runner's own code for the same oldest macOS as the archive's objects.
  MACOSX_DEPLOYMENT_TARGET="$("$CK_PYTHON_HOST" -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["python_config_vars"]["MACOSX_DEPLOYMENT_TARGET"])' "$CK_PYTHON_DIST/PYTHON.json")"
  export MACOSX_DEPLOYMENT_TARGET
  # The final link runs rust-lld (build.rs explains why), which must be able to read the SDK's
  # .tbd stubs. LLVM 23 can't read the macOS 27 SDK's; on such a machine set SDKROOT to an
  # older installed SDK, for example /Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk.
fi

cargo build --locked --release --manifest-path "$crate/Cargo.toml" --target "$target"
binary="$root/target/$target/release/ck-script-runner-python"
mkdir -p "$(dirname "$binary")"
cp "$crate/target/$target/release/ck-script-runner-python" "$binary"

if [[ "$sign" == --sign ]]; then
  [[ "$target" == *-apple-darwin ]] || fail "--sign is for macOS targets only"
  identity="${DEVELOPER_ID_APPLICATION_IDENTITY:-}"
  if [[ "$identity" == - ]]; then
    codesign --force --options runtime --sign - "$binary"
  else
    identity="$("$root/scripts/release/macos-sign-notarize.sh" unused unused --print-identity)"
    codesign --force --options runtime --timestamp --sign "$identity" "$binary"
  fi
  codesign --verify --strict --verbose=2 "$binary"
  # Every native module is linked in and nothing generates code at run time, so the runner
  # needs no entitlement; any entitlement here would be a signing mistake.
  entitlements="$(codesign -d --entitlements :- "$binary" 2>/dev/null || true)"
  [[ -z "$entitlements" ]] || fail "refusing signed runner: it carries entitlements: $entitlements"
  "$root/$crate/tests/smoke-signed.sh" "$binary" "$CK_PYTHON_HOST"
fi
