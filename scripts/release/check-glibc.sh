#!/usr/bin/env bash
# Inspect ELF version needs, not strings: embedded text is not a libc dependency.
set -euo pipefail
export LC_ALL=C

SOURCE_DIR="${1:?usage: $0 <binary-dir>}"
MAX_GLIBC=2.28

for binary in ck ck-subc ck-subc-mcp; do
  path="${SOURCE_DIR}/${binary}"
  if ! versions=$(readelf --version-info --wide "$path"); then
    echo "${binary}: cannot read ELF version requirements: ${path}" >&2
    exit 1
  fi
  # Only version needs are imports; version definitions are exports.
  required=$(printf '%s\n' "$versions" | awk '
    /^Version needs section/ { needs = 1; next }
    /^Version .* section/ { needs = 0 }
    needs { for (i = 1; i <= NF; i++) if ($i == "Name:" && $(i+1) ~ /^GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?$/) {
      version = $(i+1); sub(/^GLIBC_/, "", version); print version
    } }
  ' | sort -Vu | tail -n 1)
  # Fail closed for a missing binary, a non-ELF input or an unexpected static build.
  if [[ -z "$required" ]]; then
    echo "${binary}: no numeric GLIBC version requirements found in ${path}" >&2
    exit 1
  fi
  highest=$(printf '%s\n' "$MAX_GLIBC" "$required" | sort -V | tail -n 1)
  if [[ "$highest" != "$MAX_GLIBC" ]]; then
    echo "${binary}: requires GLIBC_${required}, newer than allowed GLIBC_${MAX_GLIBC}" >&2
    exit 1
  fi
  echo "${binary}: highest required GLIBC_${required} <= GLIBC_${MAX_GLIBC}"
done
