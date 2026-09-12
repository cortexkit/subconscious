#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
registry_json_dir="${SUBC_REGISTRY_JSON_DIR:-}"
registry_api_url="${SUBC_CRATES_IO_API_URL:-https://crates.io/api/v1/crates}"

usage() {
  echo "usage: $0 [--registry-json-dir <dir>]" >&2
}

while (($# > 0)); do
  case "$1" in
    --registry-json-dir)
      if (($# < 2)); then
        usage
        exit 2
      fi
      registry_json_dir="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      echo "refusal: unknown argument '$1'" >&2
      exit 2
      ;;
  esac
done

if [[ -n "$registry_json_dir" && ! -d "$registry_json_dir" ]]; then
  echo "refusal: registry JSON directory does not exist: $registry_json_dir" >&2
  exit 2
fi

wire_crates=(
  subc-protocol
  subc-transport
  subc-control
  subc-client-rs
)
unpublished=()
tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT

for crate in "${wire_crates[@]}"; do
  manifest="$repo_root/crates/$crate/Cargo.toml"
  version=$(awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "$manifest")
  if [[ -z "$version" ]]; then
    echo "refusal: could not read $crate version from $manifest" >&2
    exit 2
  fi

  response="$tmp_dir/$crate.json"
  if [[ -n "$registry_json_dir" ]]; then
    fixture="$registry_json_dir/$crate.json"
    status_file="$registry_json_dir/$crate.status"
    if [[ ! -f "$fixture" ]]; then
      echo "refusal: missing registry fixture for $crate: $fixture" >&2
      exit 2
    fi
    cp "$fixture" "$response"
    if [[ -f "$status_file" ]]; then
      status=$(tr -d '[:space:]' <"$status_file")
    else
      status=200
    fi
  else
    if ! status=$(curl --silent --show-error --location \
      --connect-timeout 10 --max-time 30 \
      --user-agent 'CortexKit subconscious release detector/1.0' \
      --output "$response" --write-out '%{http_code}' \
      "$registry_api_url/$crate"); then
      echo "refusal: registry request failed for $crate" >&2
      exit 2
    fi
  fi

  case "$status" in
    404)
      unpublished+=("$crate $version")
      continue
      ;;
    200) ;;
    *)
      echo "refusal: registry returned HTTP $status for $crate" >&2
      exit 2
      ;;
  esac

  if ! present=$(jq -r --arg version "$version" '
    if ((.crate.max_version | type) != "string")
      or ((.versions | type) != "array")
      or any(.versions[]; (.num | type) != "string")
    then error("malformed crates.io response")
    else (.crate.max_version == $version) or any(.versions[]; .num == $version)
    end
  ' "$response"); then
    echo "refusal: invalid registry response for $crate" >&2
    exit 2
  fi

  if [[ "$present" != "true" ]]; then
    unpublished+=("$crate $version")
  fi
done

if ((${#unpublished[@]} > 0)); then
  printf '%s\n' "${unpublished[@]}"
fi
