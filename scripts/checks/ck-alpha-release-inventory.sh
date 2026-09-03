#!/usr/bin/env bash
# Verify the alpha archives before an operator begins a fresh-machine install.
# The gate reads each archive's own sidecar; it deliberately does not use a
# release-wide manifest because installers and upgrades cannot use one either.
set -uo pipefail

readonly DEFAULT_SUBCONSCIOUS_RELEASE_URL="https://github.com/cortexkit/subconscious/releases/latest/download"
readonly DEFAULT_AFT_RELEASE_URL="https://github.com/cortexkit/aft/releases/latest/download"
readonly SUPPORTED_TUPLES=("darwin-arm64" "linux-x64" "linux-arm64" "windows-x64")
readonly SUBCONSCIOUS_BINARIES=("ck" "ck-subc" "ck-subc-mcp")
readonly AFT_BINARIES=("ck-aft")

subconscious_release_url="${CK_SUBCONSCIOUS_RELEASE_URL:-$DEFAULT_SUBCONSCIOUS_RELEASE_URL}"
aft_release_url="${CK_AFT_RELEASE_URL:-$DEFAULT_AFT_RELEASE_URL}"
failed=0
temp_dir=""

usage() {
  cat <<'EOF'
Usage: ck-alpha-release-inventory.sh [options]

Verify the alpha release inventory before recording any operator-assisted
installation. The gate downloads every required archive and its matching
per-asset SHA-256 sidecar, verifies the sidecar record, and verifies the
archive digest.

Options:
  --subconscious-release-url URL  Base download URL for ck, ck-subc, and ck-subc-mcp.
  --aft-release-url URL           Base download URL for the external ck-aft lane.
  -h, --help                      Show this help.

The same URLs may be supplied through CK_SUBCONSCIOUS_RELEASE_URL and
CK_AFT_RELEASE_URL. Point each URL at one release's download directory, for
example https://github.com/cortexkit/subconscious/releases/download/subc-core-vX.Y.Z.
EOF
}

trim_trailing_slash() {
  local url="$1"
  while [[ "$url" == */ ]]; do
    url="${url%/}"
  done
  printf '%s\n' "$url"
}

require_command() {
  local command_name="$1"
  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'inventory-error: required command unavailable: %s\n' "$command_name" >&2
    exit 2
  fi
}

sha256_for() {
  local path="$1"

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$path" | awk '{print $NF}'
  else
    return 127
  fi
}

record_missing() {
  local component="$1"
  local lane="$2"
  local asset_name="$3"
  local asset_url="$4"

  # This is intentionally a release-incomplete result. The target tuple is
  # supported, so a missing publication must not be disguised as unsupported.
  printf 'release-incomplete: component=%s lane=%s missing=%s source=%s\n' \
    "$component" "$lane" "$asset_name" "$asset_url" >&2
  failed=1
}

record_invalid() {
  local component="$1"
  local lane="$2"
  local archive_name="$3"
  local sidecar_name="$4"
  local reason="$5"

  printf 'inventory-invalid: component=%s lane=%s archive=%s sidecar=%s reason=%s\n' \
    "$component" "$lane" "$archive_name" "$sidecar_name" "$reason" >&2
  failed=1
}

download_asset() {
  local component="$1"
  local lane="$2"
  local asset_name="$3"
  local asset_url="$4"
  local destination="$5"

  if ! curl --fail --location --silent --show-error --retry 2 --connect-timeout 15 \
    --max-time 120 --output "$destination" "$asset_url"; then
    rm -f "$destination"
    record_missing "$component" "$lane" "$asset_name" "$asset_url"
    return 1
  fi

  return 0
}

expected_digest_from_sidecar() {
  local sidecar_path="$1"
  local archive_name="$2"
  local record
  local line_count

  line_count=$(awk 'END { print NR }' "$sidecar_path")
  if [[ "$line_count" != "1" ]]; then
    return 1
  fi

  record=$(<"$sidecar_path")
  record="${record%$'\r'}"
  if [[ "$record" =~ ^([[:xdigit:]]{64})[[:space:]]+\*?([^[:space:]]+)$ ]]; then
    if [[ "${BASH_REMATCH[2]}" != "$archive_name" ]]; then
      return 1
    fi
    printf '%s\n' "${BASH_REMATCH[1]}" | tr '[:upper:]' '[:lower:]'
    return 0
  fi

  return 1
}

check_asset() {
  local component="$1"
  local lane="$2"
  local release_url="$3"
  local os="$4"
  local arch="$5"
  local archive_name="${component}-${os}-${arch}.zip"
  local sidecar_name="${archive_name}.sha256"
  local archive_path="${temp_dir}/${component}-${os}-${arch}.zip"
  local sidecar_path="${archive_path}.sha256"
  local archive_downloaded=0
  local sidecar_downloaded=0
  local expected_digest
  local actual_digest

  if download_asset "$component" "$lane" "$archive_name" \
    "${release_url}/${archive_name}" "$archive_path"; then
    archive_downloaded=1
  fi
  if download_asset "$component" "$lane" "$sidecar_name" \
    "${release_url}/${sidecar_name}" "$sidecar_path"; then
    sidecar_downloaded=1
  fi

  if [[ "$archive_downloaded" -ne 1 || "$sidecar_downloaded" -ne 1 ]]; then
    return
  fi

  if ! expected_digest=$(expected_digest_from_sidecar "$sidecar_path" "$archive_name"); then
    record_invalid "$component" "$lane" "$archive_name" "$sidecar_name" "sidecar-is-not-a-matching-shasum-record"
    return
  fi
  if ! actual_digest=$(sha256_for "$archive_path"); then
    record_invalid "$component" "$lane" "$archive_name" "$sidecar_name" "archive-could-not-be-hashed"
    return
  fi
  actual_digest=$(printf '%s' "$actual_digest" | tr '[:upper:]' '[:lower:]')
  if [[ "$actual_digest" != "$expected_digest" ]]; then
    record_invalid "$component" "$lane" "$archive_name" "$sidecar_name" "sha256-mismatch"
    return
  fi

  printf 'inventory-available: component=%s lane=%s archive=%s sidecar=%s sha256=%s\n' \
    "$component" "$lane" "$archive_name" "$sidecar_name" "$actual_digest"
}

check_lane() {
  local lane="$1"
  local release_url="$2"
  shift 2
  local component
  local tuple
  local os
  local arch

  printf 'inventory-lane: name=%s source=%s\n' "$lane" "$release_url"
  for component in "$@"; do
    for tuple in "${SUPPORTED_TUPLES[@]}"; do
      os="${tuple%-*}"
      arch="${tuple#*-}"
      check_asset "$component" "$lane" "$release_url" "$os" "$arch"
    done
  done
}

while (($# > 0)); do
  case "$1" in
    --subconscious-release-url)
      [[ $# -ge 2 ]] || { printf 'inventory-error: --subconscious-release-url requires a URL\n' >&2; exit 2; }
      subconscious_release_url="$2"
      shift 2
      ;;
    --aft-release-url)
      [[ $# -ge 2 ]] || { printf 'inventory-error: --aft-release-url requires a URL\n' >&2; exit 2; }
      aft_release_url="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'inventory-error: unknown argument: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

subconscious_release_url=$(trim_trailing_slash "$subconscious_release_url")
aft_release_url=$(trim_trailing_slash "$aft_release_url")
if [[ -z "$subconscious_release_url" || -z "$aft_release_url" ]]; then
  printf 'inventory-error: release URLs must not be empty\n' >&2
  exit 2
fi

require_command curl
if ! command -v shasum >/dev/null 2>&1 \
  && ! command -v sha256sum >/dev/null 2>&1 \
  && ! command -v openssl >/dev/null 2>&1; then
  printf 'inventory-error: need shasum, sha256sum, or openssl for SHA-256 verification\n' >&2
  exit 2
fi

if ! temp_dir=$(mktemp -d "${TMPDIR:-/tmp}/ck-alpha-release-inventory.XXXXXX"); then
  printf 'inventory-error: could not create a temporary inventory directory\n' >&2
  exit 2
fi
trap 'rm -rf "$temp_dir"' EXIT

printf 'release-inventory: started supported-tuples=darwin-arm64,linux-x64,linux-arm64,windows-x64\n'
check_lane "subconscious" "$subconscious_release_url" "${SUBCONSCIOUS_BINARIES[@]}"
# AFT publication is owned by cortexkit/aft, not this repository. Keeping its
# result in a separate lane makes the external dependency visible in the gate.
check_lane "aft-external-dependency" "$aft_release_url" "${AFT_BINARIES[@]}"

if [[ "$failed" -ne 0 ]]; then
  printf 'release-inventory: release-incomplete; do not begin the operator matrix\n' >&2
  exit 1
fi

printf 'release-inventory: passed; operator matrix may begin\n'
exit 0
