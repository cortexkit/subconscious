#!/usr/bin/env bash
# Package the installable subconscious binaries with convention-derived names.
set -euo pipefail

OS="${1:?usage: $0 <darwin|linux> <arm64|x64> <source-dir> <dist-dir>}"
ARCH="${2:?usage: $0 <darwin|linux> <arm64|x64> <source-dir> <dist-dir>}"
SOURCE_DIR="${3:?usage: $0 <darwin|linux> <arm64|x64> <source-dir> <dist-dir>}"
DIST_DIR="${4:?usage: $0 <darwin|linux> <arm64|x64> <source-dir> <dist-dir>}"
BINARIES=(ck ck-subc ck-subc-mcp)

case "$OS" in
  darwin|linux) ;;
  *)
    echo "unsupported archive OS: ${OS}" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  arm64|x64) ;;
  *)
    echo "unsupported archive architecture: ${ARCH}" >&2
    exit 1
    ;;
esac

command -v zip >/dev/null || {
  echo "zip is required to package release archives" >&2
  exit 1
}
command -v shasum >/dev/null || {
  echo "shasum is required to create release digest sidecars" >&2
  exit 1
}

# The shipped ck must not carry the test-only release-index key path: a build
# whose feature set was unified from a test invocation would honour an
# environment-supplied verifying key, turning the signature check into
# configuration. The binary answers for its own build shape; refuse otherwise.
shape="$("${SOURCE_DIR}/ck" --ck-build-shape 2>/dev/null || true)"
if [[ "$shape" != "test-support: off" ]]; then
  echo "refusing to package ck: build shape is '${shape}', expected 'test-support: off'" >&2
  exit 1
fi

mkdir -p "$DIST_DIR"
for binary in "${BINARIES[@]}"; do
  source_path="${SOURCE_DIR}/${binary}"
  archive_name="${binary}-${OS}-${ARCH}.zip"
  archive_path="${DIST_DIR}/${archive_name}"

  if [[ ! -f "$source_path" ]]; then
    echo "missing release binary: ${source_path}" >&2
    exit 1
  fi

  rm -f "$archive_path" "${archive_path}.sha256"
  zip -q -j "$archive_path" "$source_path"
  # Generate from within dist so `shasum -c` receives the exact published name,
  # not a runner-local directory prefix.
  (
    cd "$DIST_DIR"
    shasum -a 256 "$archive_name" > "${archive_name}.sha256"
  )
done
