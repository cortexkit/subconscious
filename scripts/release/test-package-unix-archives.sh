#!/usr/bin/env bash
set -euo pipefail
finished=

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="$(mktemp -d "${SCRIPT_DIR}/.test-release-package.XXXXXX")"
finish() {
  local rc=$?
  rm -rf "$FIXTURE_DIR"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "test-package-unix-archives.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

mkdir -p "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist"
for binary in ck-subc ck-subc-mcp; do
  printf '%s fixture\n' "$binary" > "$FIXTURE_DIR/source/$binary"
done
# ck must answer for its own build shape; the packager refuses a ck that
# carries the test-only release-index key path.
write_ck_fixture() {
  printf '#!/bin/sh\ncase "$1" in --ck-build-shape) echo "test-support: %s";; esac\n' "$1" \
    > "$FIXTURE_DIR/source/ck"
  chmod 755 "$FIXTURE_DIR/source/ck"
}

write_ck_fixture on
if "${SCRIPT_DIR}/package-unix-archives.sh" linux x64 "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist" 2>/dev/null; then
  echo "packager accepted a ck built with test-support" >&2
  exit 1
fi
[[ ! -e "$FIXTURE_DIR/dist/ck-linux-x64.zip" ]]

write_ck_fixture off
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
finished=1
