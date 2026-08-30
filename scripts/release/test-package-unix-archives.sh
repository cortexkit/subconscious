#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="$(mktemp -d "${SCRIPT_DIR}/.test-release-package.XXXXXX")"
trap 'rm -rf "$FIXTURE_DIR"' EXIT

mkdir -p "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist"
for binary in ck ck-subc ck-subc-mcp; do
  printf '%s fixture\n' "$binary" > "$FIXTURE_DIR/source/$binary"
done

"${SCRIPT_DIR}/package-unix-archives.sh" linux x64 "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist"

for binary in ck ck-subc ck-subc-mcp; do
  archive_name="${binary}-linux-x64.zip"
  archive_path="$FIXTURE_DIR/dist/$archive_name"
  sidecar_path="${archive_path}.sha256"

  [[ "$(unzip -Z1 "$archive_path")" == "$binary" ]]
  [[ "$(wc -l < "$sidecar_path" | tr -d ' ')" == "1" ]]
  grep -Eq "^[[:xdigit:]]{64}  ${archive_name}$" "$sidecar_path"
  (
    cd "$FIXTURE_DIR/dist"
    shasum -a 256 -c "${archive_name}.sha256"
  )
done
