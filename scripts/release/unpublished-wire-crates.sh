#!/usr/bin/env bash
set -euo pipefail
finished=

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
registry_json_dir="${SUBC_REGISTRY_JSON_DIR:-}"
registry_api_url="${SUBC_CRATES_IO_API_URL:-https://crates.io/api/v1/crates}"
semver_candidates=0
base=
only_crate=

usage() {
  echo "usage: $0 [--registry-json-dir <dir>] [--semver-candidates [--base <commit> | --crate <name>]]" >&2
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
    --semver-candidates)
      semver_candidates=1
      shift
      ;;
    --base|--crate)
      if (($# < 2)) || [[ -z "$2" ]]; then
        usage
        exit 2
      fi
      case "$1" in
        --base) base="$2" ;;
        --crate) only_crate="$2" ;;
      esac
      shift 2
      ;;
    -h|--help)
      usage
      finished=1
      exit 0
      ;;
    *)
      usage
      echo "refusal: unknown argument '$1'" >&2
      exit 2
      ;;
  esac
done

if [[ -n "$base$only_crate" ]] && (( ! semver_candidates )); then
  usage
  exit 2
fi
if [[ -n "$base" && -n "$only_crate" ]]; then
  usage
  exit 2
fi
if [[ -n "$registry_json_dir" && ! -d "$registry_json_dir" ]]; then
  echo "refusal: registry JSON directory does not exist: $registry_json_dir" >&2
  exit 2
fi

# Every published crate, in dependency order: a crate is listed after
# everything it depends on, which is the order the publish jobs run in.
wire_crates=(
  subc-config
  subc-protocol
  subc-os
  subc-transport
  subc-control
  subc-client-rs
  subc-jsonc
  subc-cgroup
  subc-uptime
  subc-jobobject
  subc-presence
  subc-daemon
  cortexkit-bus-testkit
)
if [[ -n "$only_crate" ]]; then
  # The tag-triggered workflow supplies an explicit package name to publish-crate.yml.
  if [[ ! "$only_crate" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_-]*$ ]]; then
    echo "refusal: invalid crate name '$only_crate'" >&2
    exit 2
  fi
  wire_crates=("$only_crate")
fi
if [[ -n "$base" ]]; then
  base=$(git -C "$repo_root" rev-parse --verify --end-of-options "${base}^{commit}")
fi

manifest_version() {
  awk -F'"' '!found && /^version[[:space:]]*=/ { print $2; found=1 }' "$@"
}

unpublished=()
tmp_dir=$(mktemp -d)
finish() {
  local rc=$?
  rm -rf "$tmp_dir"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "unpublished-wire-crates.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

for crate in "${wire_crates[@]}"; do # nonempty-array: literal published crate list
  manifest="crates/$crate/Cargo.toml"
  version=$(manifest_version "$repo_root/$manifest")
  if [[ -z "$version" ]]; then
    echo "refusal: could not read $crate version from $manifest" >&2
    exit 2
  fi
  if [[ -n "$base" ]]; then
    base_entry=$(git -C "$repo_root" ls-tree "$base" -- "$manifest")
    if [[ -n "$base_entry" ]]; then
      base_version=$(git -C "$repo_root" show "$base:$manifest" | manifest_version)
      if [[ -z "$base_version" ]]; then
        echo "refusal: could not read $crate version at $base" >&2
        exit 2
      fi
      if [[ "$version" == "$base_version" ]]; then
        continue
      fi
    fi
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
      if ((semver_candidates)); then
        echo "Skipping $crate $version: no crates.io baseline (HTTP 404)" >&2
      else
        unpublished+=("$crate $version")
      fi
      continue
      ;;
    200) ;;
    *)
      echo "refusal: registry returned HTTP $status for $crate" >&2
      exit 2
      ;;
  esac

  if ! registry_state=$(jq -er --arg version "$version" '
    if ((.crate.max_version | type) != "string")
      or (.crate.max_version == "")
      or ((.versions | type) != "array")
      or any(.versions[]; (.num | type) != "string")
    then error("malformed crates.io response")
    else [((.crate.max_version == $version) or any(.versions[]; .num == $version)),
          .crate.max_version] | @tsv
    end
  ' "$response"); then
    echo "refusal: invalid registry response for $crate" >&2
    exit 2
  fi

  read -r present baseline <<< "$registry_state"
  if [[ "$present" != "true" ]]; then
    if ((semver_candidates)); then
      unpublished+=("$crate $version $baseline")
    else
      unpublished+=("$crate $version")
    fi
  fi
done

if ((${#unpublished[@]} > 0)); then
  printf '%s\n' "${unpublished[@]}" # nonempty-array: positive length guard
fi
finished=1
