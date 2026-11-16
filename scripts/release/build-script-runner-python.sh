#!/usr/bin/env bash
# Build ck-script-runner-python for one Rust target from its pinned CPython.
#
#   build-script-runner-python.sh <rust-target> [--sign] [--test]
#
# On macOS and Linux, the archive named for <rust-target> in
# clients/script-runner-python/python-archives.lock is downloaded once into
# target/python-dist/archives/, checked against the pinned size and SHA-256, and extracted into
# target/python-dist/<rust-target>/. On Windows, where no archive can make one self-contained
# runner, the sources in clients/script-runner-python/windows-sources.lock are downloaded and
# checked the same way, and tools/build_windows_cpython.py builds CPython statically into the
# same layout. Cargo's build script only reads that copy (CK_PYTHON_DIST); it never downloads.
#
# The pure-Python standard library is compiled to bytecode by CK_PYTHON_HOST, a CPython 3.13 that
# runs on the build machine. For a native build it defaults to the archive's interpreter (or the
# python.exe the Windows build makes); a cross build must name one. On Windows the CPython build
# itself runs with CK_BUILD_PYTHON (default: python on PATH), any Python 3.10 or newer.
#
# --sign (macOS only) signs the binary with the hardened runtime and no entitlements, checks the
# signature, and runs the signed copy. DEVELOPER_ID_APPLICATION_IDENTITY=- signs ad hoc, as CI
# does; otherwise the identity comes from macos-sign-notarize.sh's own resolution.
#
# --test then runs the runner's tests against the same build.
set -euo pipefail

usage="usage: $0 <rust-target> [--sign] [--test]"
target="${1:?$usage}"
shift
sign="" test=""
for option in "$@"; do
  case "$option" in
    --sign) sign=--sign ;;
    --test) test=--test ;;
    *) echo "$usage" >&2; exit 2 ;;
  esac
done
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
crate="clients/script-runner-python"
lock="$crate/python-archives.lock"

fail() {
  echo "$*" >&2
  exit 1
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# fetch <file> <size> <sha256> <url> <lock>: downloads once, refusing any other bytes.
fetch() {
  local file="$1" size="$2" sha="$3" url="$4" pins="$5"
  matches() { [[ -f "$1" && "$(wc -c < "$1" | tr -d ' ')" == "$size" && "$(sha256 "$1")" == "$sha" ]]; }
  matches "$file" && return 0
  mkdir -p "$(dirname "$file")"
  rm -f "$file.part"
  curl -fsSL --retry 3 -o "$file.part" "$url"
  if ! matches "$file.part"; then
    rm -f "$file.part"
    fail "refusing $url: its size or SHA-256 differs from $pins"
  fi
  mv "$file.part" "$file"
}

# Native Windows programs want Windows paths in arguments and the environment.
native_path() {
  if command -v cygpath >/dev/null 2>&1; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
}

dist="$root/target/python-dist"
extracted="$dist/$target"
host="$(rustc -vV | sed -n 's/^host: //p')"
exe=""
case "$target" in
  *-pc-windows-msvc)
    # No usable archive exists for Windows, so CPython is built from pinned sources.
    exe=".exe"
    sources_lock="$crate/windows-sources.lock"
    pins="$(grep -v '^[[:space:]]*#' "$sources_lock")"
    while read -r name size sha url; do
      [[ -z "$name" ]] && continue
      suffix="${url##*/}"
      case "$suffix" in
        *.tar.xz) suffix=tar.xz ;;
        *.tar.gz) suffix=tar.gz ;;
        *.zip) suffix=zip ;;
        *) fail "unknown archive type: $url" ;;
      esac
      fetch "$dist/windows-sources/$name.$suffix" "$size" "$sha" "$url" "$sources_lock"
    done <<< "$pins"
    # The stamp records which sources and build recipe produced the tree, so a change to
    # either rebuilds it instead of linking stale objects.
    stamp="$(cat "$sources_lock" "$crate/tools/build_windows_cpython.py" | sha256 /dev/stdin)"
    work="$root/target/python-build/$target"
    if [[ "$(cat "$extracted/.build-stamp" 2>/dev/null || true)" != "$stamp" ]]; then
      rm -rf "$extracted" "$work"
      mkdir -p "$extracted"
      "${CK_BUILD_PYTHON:-python}" "$(native_path "$crate/tools/build_windows_cpython.py")" "$target" \
        "$(native_path "$dist/windows-sources")" "$(native_path "$work")" "$(native_path "$extracted/python")"
      echo "$stamp" > "$extracted/.build-stamp"
    fi
    if [[ -z "${CK_PYTHON_HOST:-}" ]]; then
      [[ "$host" == "$target" ]] || fail "cross build for $target on $host: set CK_PYTHON_HOST to a CPython 3.13 that runs here"
      arch=amd64
      [[ "$target" == aarch64-* ]] && arch=arm64
      CK_PYTHON_HOST="$work/cpython/PCbuild/$arch/python.exe"
    fi
    # Cargo runs from the crate folder, so a relative interpreter path must be made absolute.
    case "$CK_PYTHON_HOST" in
      /* | [A-Za-z]:*) ;;
      *) CK_PYTHON_HOST="$root/$CK_PYTHON_HOST" ;;
    esac
    CK_PYTHON_HOST="$(native_path "$CK_PYTHON_HOST")"
    CK_PYTHON_DIST="$(native_path "$extracted/python")"
    ;;
  *)
    lock="$crate/python-archives.lock"
    size="" sha="" url=""
    read -r size sha url < <(awk -v target="$target" '$1 == target { print $2, $3, $4 }' "$lock") || true
    [[ -n "$url" ]] || fail "no CPython archive is pinned for $target in $lock"
    archive="$dist/archives/$target.tar.zst"
    fetch "$archive" "$size" "$sha" "$url" "$lock"
    # The stamp records which pinned archive the extracted tree came from, so a pin change
    # re-extracts instead of building against stale files.
    if [[ "$(cat "$extracted/.archive-sha256" 2>/dev/null || true)" != "$sha" ]]; then
      rm -rf "$extracted"
      mkdir -p "$extracted"
      zstd -dc "$archive" | tar -xf - -C "$extracted"
      echo "$sha" > "$extracted/.archive-sha256"
    fi
    if [[ -z "${CK_PYTHON_HOST:-}" ]]; then
      [[ "$host" == "$target" ]] || fail "cross build for $target on $host: set CK_PYTHON_HOST to a CPython 3.13 that runs here"
      CK_PYTHON_HOST="$extracted/python/install/bin/python3"
    fi
    CK_PYTHON_DIST="$extracted/python"
    ;;
esac
export CK_PYTHON_DIST
export CK_PYTHON_HOST
if [[ "$target" == *-apple-darwin ]]; then
  # Build the runner's own code for the same oldest macOS as the archive's objects.
  MACOSX_DEPLOYMENT_TARGET="$("$CK_PYTHON_HOST" -I -c 'import json, sys; print(json.load(open(sys.argv[1]))["python_config_vars"]["MACOSX_DEPLOYMENT_TARGET"])' "$CK_PYTHON_DIST/PYTHON.json")"
  export MACOSX_DEPLOYMENT_TARGET
  # The final link runs rust-lld (build.rs explains why), which must be able to read the SDK's
  # .tbd stubs. LLVM 23 can't read the macOS 27 SDK's; on such a machine set SDKROOT to an
  # older installed SDK, for example /Library/Developer/CommandLineTools/SDKs/MacOSX26.sdk.
fi

# Cargo runs from the crate folder so its .cargo/config.toml applies (the static C runtime on
# Windows).
(cd "$crate" && cargo build --locked --release --target "$target")
target_dir="$(cd "$crate" && cargo metadata --locked --no-deps --format-version 1 |
  "$CK_PYTHON_HOST" -I -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
built="$target_dir/$target/release/ck-script-runner-python$exe"
binary="$root/target/$target/release/ck-script-runner-python$exe"
mkdir -p "$(dirname "$binary")"
if [[ "$built" -ef "$binary" ]]; then
  : # CARGO_TARGET_DIR already points at the repository's target folder.
else
  cp "$built" "$binary"
fi

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

if [[ "$test" == --test ]]; then
  if [[ -n "$exe" && -z "${CK_HARNESS_PYTHON:-}" ]]; then
    # The pipe harness needs ctypes, which the static Windows python.exe leaves out.
    CK_HARNESS_PYTHON="$(native_path "$(command -v "${CK_BUILD_PYTHON:-python}")")"
    export CK_HARNESS_PYTHON
  fi
  (cd "$crate" && cargo test --locked --release --target "$target" --workspace)
fi
