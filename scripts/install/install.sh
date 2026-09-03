#!/usr/bin/env bash
# Bootstrap only ck from the CortexKit release index. Setup owns all runtime
# and configuration work, so this script must never invoke `ck setup`. The
# index signature is not checked here: bootstrap trust is TLS to the index
# host, and the placed `ck` verifies the signed index on its first setup.
set -euo pipefail

readonly INDEX_URL_DEFAULT="https://cortexkit.io/releases/v1/index.json"
readonly PATH_BLOCK_BEGIN="# cortexkit-managed PATH begin"
readonly PATH_BLOCK_END="# cortexkit-managed PATH end"

refuse() {
  local refusal_type="$1"
  local evidence="$2"
  printf 'refusal: %s: %s\n' "$refusal_type" "$evidence" >&2
  exit 1
}

sha256_for() {
  local file="$1"
  local digest

  if command -v shasum >/dev/null 2>&1; then
    if ! digest=$(shasum -a 256 "$file" | awk '{print $1}'); then
      refuse "digest-verification-failed" "shasum could not hash $file"
    fi
  elif command -v sha256sum >/dev/null 2>&1; then
    if ! digest=$(sha256sum "$file" | awk '{print $1}'); then
      refuse "digest-verification-failed" "sha256sum could not hash $file"
    fi
  elif command -v openssl >/dev/null 2>&1; then
    if ! digest=$(openssl dgst -sha256 "$file" | awk '{print $NF}'); then
      refuse "digest-verification-failed" "openssl could not hash $file"
    fi
  else
    refuse "digest-verification-unavailable" "need shasum, sha256sum, or openssl"
  fi

  printf '%s\n' "$digest"
}

json_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

select_profile() {
  if [[ -n "${CK_PROFILE_PATH:-}" ]]; then
    profile_path="$CK_PROFILE_PATH"
    profile_kind="posix"
    return
  fi

  case "${SHELL##*/}" in
    zsh)
      profile_path="$HOME/.zshrc"
      profile_kind="posix"
      ;;
    fish)
      profile_path="${XDG_CONFIG_HOME:-$HOME/.config}/fish/config.fish"
      profile_kind="fish"
      ;;
    *)
      # Bash is the portable fallback for curl|sh, where $SHELL may be unset.
      profile_path="$HOME/.bashrc"
      profile_kind="posix"
      ;;
  esac
}

ensure_profile_path() {
  local profile="$1"
  local profile_block="$2"
  local profile_directory
  local without_block
  local updated_profile

  profile_directory=$(dirname "$profile")
  if ! mkdir -p "$profile_directory"; then
    refuse "path-update-failed" "could not create profile directory $profile_directory"
  fi
  if [[ ! -e "$profile" ]]; then
    if ! : >"$profile"; then
      refuse "path-update-failed" "could not create shell profile $profile"
    fi
  fi
  if [[ ! -f "$profile" ]]; then
    refuse "path-update-failed" "$profile is not a regular file"
  fi

  if ! without_block=$(mktemp "${profile}.without-cortexkit.XXXXXX"); then
    refuse "path-update-failed" "could not prepare update for $profile"
  fi
  if ! updated_profile=$(mktemp "${profile}.with-cortexkit.XXXXXX"); then
    rm -f "$without_block"
    refuse "path-update-failed" "could not prepare update for $profile"
  fi

  if ! awk -v begin="$PATH_BLOCK_BEGIN" -v end="$PATH_BLOCK_END" '
    $0 == begin {
      if (inside) {
        invalid = 1
        exit 1
      }
      inside = 1
      begins++
      next
    }
    $0 == end {
      if (!inside) {
        invalid = 1
        exit 1
      }
      inside = 0
      ends++
      next
    }
    !inside { print }
    END {
      if (invalid || inside || begins != ends || begins > 1) {
        exit 1
      }
    }
  ' "$profile" >"$without_block"; then
    rm -f "$without_block" "$updated_profile"
    refuse "path-update-failed" "$profile has malformed CortexKit PATH markers"
  fi

  if [[ -s "$without_block" ]]; then
    if ! cat "$without_block" >"$updated_profile"; then
      rm -f "$without_block" "$updated_profile"
      refuse "path-update-failed" "could not update shell profile $profile"
    fi
    if ! printf '\n' >>"$updated_profile"; then
      rm -f "$without_block" "$updated_profile"
      refuse "path-update-failed" "could not update shell profile $profile"
    fi
  fi
  if ! printf '%s\n' "$profile_block" >>"$updated_profile"; then
    rm -f "$without_block" "$updated_profile"
    refuse "path-update-failed" "could not update shell profile $profile"
  fi
  rm -f "$without_block"

  if cmp -s "$profile" "$updated_profile"; then
    rm -f "$updated_profile"
    return
  fi
  if ! mv "$updated_profile" "$profile"; then
    rm -f "$updated_profile"
    refuse "path-update-failed" "could not replace shell profile $profile"
  fi
}

write_manifest() {
  local manifest="$1"
  local binary="$2"
  local binary_digest="$3"
  local profile="$4"
  local manifest_tmp
  local escaped_manifest
  local escaped_binary
  local escaped_digest
  local escaped_profile

  if ! manifest_tmp=$(mktemp "${manifest}.XXXXXX"); then
    refuse "inventory-record-failed" "could not prepare $manifest"
  fi

  escaped_manifest=$(json_escape "$manifest")
  escaped_binary=$(json_escape "$binary")
  escaped_digest=$(json_escape "$binary_digest")
  escaped_profile=$(json_escape "$profile")
  if ! cat >"$manifest_tmp" <<EOF
{
  "schema_version": 1,
  "installer": "ck",
  "platform": "${os}-${arch}",
  "mutations": [
    {
      "kind": "binary-placement",
      "path": "$escaped_binary",
      "sha256": "$escaped_digest"
    },
    {
      "kind": "shell-profile-path",
      "path": "$escaped_profile",
      "begin_marker": "$PATH_BLOCK_BEGIN",
      "end_marker": "$PATH_BLOCK_END"
    },
    {
      "kind": "ownership-record",
      "path": "$escaped_manifest"
    }
  ]
}
EOF
  then
    rm -f "$manifest_tmp"
    refuse "inventory-record-failed" "could not write $manifest"
  fi

  if [[ -f "$manifest" ]] && cmp -s "$manifest" "$manifest_tmp"; then
    rm -f "$manifest_tmp"
    return
  fi
  if ! mv "$manifest_tmp" "$manifest"; then
    rm -f "$manifest_tmp"
    refuse "inventory-record-failed" "could not replace $manifest"
  fi
}

if ! raw_os=$(uname -s); then
  refuse "unsupported-platform" "could not determine operating system"
fi
if ! raw_arch=$(uname -m); then
  refuse "unsupported-platform" "could not determine architecture"
fi
case "$raw_os" in
  Darwin) os="darwin" ;;
  Linux) os="linux" ;;
  *) refuse "unsupported-platform" "${raw_os}-${raw_arch}; supported tuples are darwin-arm64, linux-x64, windows-x64" ;;
esac
case "$raw_arch" in
  arm64|aarch64) arch="arm64" ;;
  x86_64|amd64) arch="x64" ;;
  *) arch="$raw_arch" ;;
esac
case "${os}-${arch}" in
  darwin-arm64|linux-x64|linux-arm64) ;;
  *) refuse "unsupported-platform" "${os}-${arch}; supported tuples are darwin-arm64, linux-x64, linux-arm64, windows-x64" ;;
esac

if [[ "$os" == "linux" ]]; then
  if ! kernel_release=$(uname -r); then
    kernel_release="unknown"
  fi
  if [[ -n "${WSL_DISTRO_NAME:-}" || "$kernel_release" == *[Mm]icrosoft* || "$kernel_release" == *WSL* ]]; then
    if ! command -v systemctl >/dev/null 2>&1; then
      refuse "wsl-systemd-user-unavailable" "systemctl is not available for the Linux installation path"
    fi
    if ! systemd_evidence=$(mktemp); then
      refuse "wsl-systemd-user-unavailable" "could not collect systemd-user prerequisite evidence"
    fi
    if ! systemctl --user show-environment >"$systemd_evidence" 2>&1; then
      printf 'refusal: wsl-systemd-user-unavailable: systemctl --user show-environment failed\n' >&2
      cat "$systemd_evidence" >&2
      rm -f "$systemd_evidence"
      exit 1
    fi
    rm -f "$systemd_evidence"
  fi
fi

if ! command -v curl >/dev/null 2>&1; then
  refuse "download-tool-unavailable" "curl is required to download the release index and archive"
fi
if ! command -v unzip >/dev/null 2>&1; then
  refuse "extraction-failed" "unzip is required to extract the release archive"
fi

index_url="${CK_RELEASE_INDEX_URL:-$INDEX_URL_DEFAULT}"
archive_name="ck-${os}-${arch}.zip"
data_dir="$HOME/.local/share/cortexkit"
bin_dir="$data_dir/bin"
destination="$bin_dir/ck"
manifest="$data_dir/installer-manifest.json"

if ! temp_dir=$(mktemp -d); then
  refuse "download-failed" "could not create a temporary download directory"
fi
trap 'rm -rf "$temp_dir"' EXIT HUP INT TERM
archive_path="$temp_dir/$archive_name"
extract_dir="$temp_dir/extracted"
index_path="$temp_dir/index.json"

if ! curl --fail --location --retry 3 --silent --show-error \
  "$index_url" --output "$index_path"; then
  refuse "release-incomplete" "release index unavailable: $index_url"
fi

# jq is not present on a fresh machine. Brace-match nested objects so a later
# component's same target cannot be mistaken for core's ck asset.
ck_fields=$(awk -v target="${os}-${arch}" '
  function extract_object(json, key,    start, i, depth, in_str, esc, c) {
    start = index(json, "\"" key "\":")
    if (start == 0) return ""
    i = start + length(key) + 3
    while (i <= length(json) && substr(json, i, 1) ~ /[ \t]/) i++
    if (substr(json, i, 1) != "{") return ""
    depth = 1; in_str = 0; esc = 0; i++
    start = i
    while (i <= length(json) && depth > 0) {
      c = substr(json, i, 1)
      if (in_str) {
        if (esc) esc = 0
        else if (c == "\\") esc = 1
        else if (c == "\"") in_str = 0
      } else {
        if (c == "\"") in_str = 1
        else if (c == "{") depth++
        else if (c == "}") depth--
      }
      i++
    }
    return substr(json, start, i - start - 1)
  }
  function json_string(obj, key,    rest) {
    if (!index(obj, "\"" key "\":")) return ""
    rest = substr(obj, index(obj, "\"" key "\":") + length(key) + 3)
    while (substr(rest, 1, 1) ~ /[ \t]/) rest = substr(rest, 2)
    if (substr(rest, 1, 1) != "\"") return ""
    rest = substr(rest, 2)
    return substr(rest, 1, index(rest, "\"") - 1)
  }
  BEGIN { RS = "" }
  {
    core = extract_object($0, "core")
    assets = extract_object(core, "assets")
    host = extract_object(assets, target)
    ck = extract_object(host, "ck")
    url = json_string(ck, "url")
    sha = json_string(ck, "sha256")
    if (url == "" || sha == "") exit 1
    print url
    print sha
  }
' "$index_path") || refuse "release-incomplete" "ck asset for ${os}-${arch} is missing from $index_url"

archive_url=$(printf '%s\n' "$ck_fields" | sed -n '1p')
expected_digest=$(printf '%s\n' "$ck_fields" | sed -n '2p' | tr 'A-F' 'a-f')
if [[ ${#expected_digest} -ne 64 || "$expected_digest" == *[!0-9a-f]* ]]; then
  refuse "release-incomplete" "ck sha256 for ${os}-${arch} is missing from $index_url"
fi

if ! curl --fail --location --retry 3 --silent --show-error \
  "$archive_url" --output "$archive_path"; then
  refuse "release-incomplete" "ck archive unavailable: $archive_name from $archive_url"
fi
actual_digest=$(sha256_for "$archive_path")
if [[ "$actual_digest" != "$expected_digest" ]]; then
  refuse "digest-mismatch" "$archive_name expected $expected_digest but downloaded $actual_digest"
fi

if ! mkdir -p "$extract_dir"; then
  refuse "extraction-failed" "could not create extraction directory for $archive_name"
fi
if ! unzip -q "$archive_path" -d "$extract_dir"; then
  refuse "extraction-failed" "could not extract $archive_name"
fi
candidate="$extract_dir/ck"
if [[ ! -f "$candidate" ]]; then
  refuse "extraction-failed" "$archive_name did not contain ck at its archive root"
fi
if ! chmod u+x "$candidate"; then
  refuse "extraction-failed" "could not make extracted ck executable"
fi
candidate_digest=$(sha256_for "$candidate")

if [[ -f "$destination" ]] && cmp -s "$candidate" "$destination"; then
  printf 'ck already matches verified download at %s; skipping placement.\n' "$destination"
else
  if ! mkdir -p "$bin_dir"; then
    refuse "placement-failed" "could not create destination directory $bin_dir"
  fi
  if ! placement_tmp=$(mktemp "$bin_dir/.ck.XXXXXX"); then
    refuse "placement-failed" "could not prepare destination $destination"
  fi
  if ! cp "$candidate" "$placement_tmp"; then
    rm -f "$placement_tmp"
    refuse "placement-failed" "could not copy ck to $destination"
  fi
  if ! chmod 755 "$placement_tmp"; then
    rm -f "$placement_tmp"
    refuse "placement-failed" "could not make $destination executable"
  fi
  if ! mv "$placement_tmp" "$destination"; then
    rm -f "$placement_tmp"
    refuse "placement-failed" "could not place ck at $destination"
  fi
  printf 'Installed ck at %s.\n' "$destination"
fi

select_profile
if [[ "$profile_kind" == "fish" ]]; then
  path_block="$PATH_BLOCK_BEGIN
set -gx PATH \"\$HOME/.local/share/cortexkit/bin\" \$PATH
$PATH_BLOCK_END"
else
  path_block="$PATH_BLOCK_BEGIN
export PATH=\"\$HOME/.local/share/cortexkit/bin:\$PATH\"
$PATH_BLOCK_END"
fi
ensure_profile_path "$profile_path" "$path_block"
write_manifest "$manifest" "$destination" "$candidate_digest" "$profile_path"

printf 'Next: ck setup\n'
