#!/usr/bin/env bash
# Bootstrap only ck from the CortexKit release index. Setup owns all runtime
# and configuration work, so this script must never invoke `ck setup`. The
# index signature is not checked here: bootstrap trust is TLS to the index
# host, and the placed `ck` verifies the signed index on its first setup.
#
# This is a bash script and the documented invocation pipes it into bash.
# Piped into `sh` on Debian and Ubuntu it runs under dash, where the very
# next line (`set -o pipefail`) is a fatal "Illegal option" before anything
# else executes, so the guard must come first and must itself be POSIX.
if [ -z "${BASH_VERSION:-}" ]; then
  printf 'refusal: wrong-shell: this installer needs bash; run: curl -fsSL https://cortexkit.io/install | bash\n' >&2
  exit 64
fi

# Fix every subsequent file creation independently of the caller's umask.
umask 077

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

# Create missing parents with an explicit mode; the caller's umask cannot
# widen a new directory beyond the mode requested here.
ensure_created_directory_tree() {
  local directory="$1"
  local mode="$2"
  local refusal_type="${3:-placement-failed}"
  local parent

  if [[ -d "$directory" ]]; then
    return
  fi
  parent=$(dirname "$directory")
  if [[ "$parent" != "$directory" && ! -d "$parent" ]]; then
    ensure_created_directory_tree "$parent" 700 "$refusal_type"
  fi
  if ! mkdir -m "$mode" "$directory"; then
    refuse "$refusal_type" "could not create directory $directory"
  fi
}

# Path-based chmod is safe only inside a private stage that other users cannot
# replace; verify both its permissions and owner before using it.
is_private_staging_directory() {
  local directory="$1"
  local mode
  local owner
  local expected_owner

  [[ -d "$directory" && ! -L "$directory" ]] || return 1
  mode=$(stat -f '%Lp' "$directory" 2>/dev/null) || mode=$(stat -c '%a' "$directory" 2>/dev/null) || return 1
  owner=$(stat -f '%u' "$directory" 2>/dev/null) || owner=$(stat -c '%u' "$directory" 2>/dev/null) || return 1
  expected_owner=$(id -u) || return 1
  [[ "$mode" == "700" && "$owner" == "$expected_owner" ]]
}

is_owned_nonwritable_directory() {
  local directory="$1"
  local mode
  local owner
  local expected_owner
  local permission_bits
  local group_mode
  local other_mode

  [[ -d "$directory" && ! -L "$directory" ]] || return 1
  mode=$(stat -f '%Lp' "$directory" 2>/dev/null) || mode=$(stat -c '%a' "$directory" 2>/dev/null) || return 1
  owner=$(stat -f '%u' "$directory" 2>/dev/null) || owner=$(stat -c '%u' "$directory" 2>/dev/null) || return 1
  expected_owner=$(id -u) || return 1
  [[ "$owner" == "$expected_owner" ]] || return 1
  case "$mode" in
    ''|*[!0-7]*) return 1 ;;
  esac
  permission_bits="${mode: -3}"
  group_mode="${permission_bits:1:1}"
  other_mode="${permission_bits:2:1}"
  case "$group_mode$other_mode" in
    *[2367]*) return 1 ;;
  esac
  return 0
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
  local link_target
  local links_followed=0

  # Replace the dotfiles target, never the link the shell reads. readlink
  # without -f works on BSD as well as GNU hosts and retains relative targets.
  while [[ -L "$profile" ]]; do
    links_followed=$((links_followed + 1))
    if [[ "$links_followed" -gt 40 ]] || ! link_target=$(readlink "$profile"); then
      refuse "path-update-failed" "could not resolve shell profile symlink $profile"
    fi
    case "$link_target" in
      /*) profile="$link_target" ;;
      *) profile="$(dirname "$profile")/$link_target" ;;
    esac
  done

  profile_directory=$(dirname "$profile")
  ensure_created_directory_tree "$profile_directory" 700 path-update-failed
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
  # Keep the replacement owner-only while constructing it; copying metadata
  # from a previously writable profile would expose the temporary contents.
  if ! : >"$updated_profile"; then
    rm -f "$without_block" "$updated_profile"
    refuse "path-update-failed" "could not preserve shell profile metadata for $profile"
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

# Two digests, two meanings: `sha256` is the placed binary's bytes (the
# ownership proof uninstall compares before deleting), `archive_sha256` is
# the release archive the binary came from (the digest the index carries,
# which currency checks compare against). Recording only the first made
# every release-placed binary read as an available update forever.
#
# On a machine that already has a manifest, `ck setup` has extended it with
# rows for the daemon, the modules, and the runtime definition, and this
# script has no JSON parser to merge into that and must never run the ck
# it just placed. Rewriting the file dropped every one of those rows, after
# which `ck upgrade` reported the daemon as not installed. So an existing
# manifest is left untouched and the rows go to a sidecar beside it, which
# the next ck to load the inventory folds in and removes.
write_manifest() {
  local manifest="$1"
  local binary="$2"
  local binary_digest="$3"
  local profile="$4"
  local archive_digest="$5"
  local target="$manifest"
  local manifest_tmp
  local escaped_manifest
  local escaped_binary
  local escaped_digest
  local escaped_archive_digest
  local escaped_profile

  if [[ -f "$manifest" ]]; then
    target="${manifest%/*}/installer-manifest.bootstrap.json"
  fi
  if ! manifest_tmp=$(mktemp "${target}.XXXXXX"); then
    refuse "inventory-record-failed" "could not prepare $target"
  fi

  escaped_manifest=$(json_escape "$manifest")
  escaped_binary=$(json_escape "$binary")
  escaped_digest=$(json_escape "$binary_digest")
  escaped_archive_digest=$(json_escape "$archive_digest")
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
      "sha256": "$escaped_digest",
      "archive_sha256": "$escaped_archive_digest"
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
    refuse "inventory-record-failed" "could not write $target"
  fi

  if [[ -f "$target" ]] && cmp -s "$target" "$manifest_tmp"; then
    rm -f "$manifest_tmp"
    return
  fi
  if ! mv "$manifest_tmp" "$target"; then
    rm -f "$manifest_tmp"
    refuse "inventory-record-failed" "could not replace $target"
  fi
}

cleanup_staging() {
  rm -rf "$temp_dir"
  if [[ -n "${placement_stage:-}" ]]; then
    rm -rf "$placement_stage"
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
  if ! command -v systemctl >/dev/null 2>&1; then
    refuse "systemd-unavailable" "Linux setup is unsupported because systemd is not available on this host"
  fi
  if ! systemd_evidence=$(mktemp); then
    refuse "systemd-user-manager-unavailable" "could not collect systemd user-manager check output"
  fi
  if ! systemctl --user show-environment >"$systemd_evidence" 2>&1; then
    rm -f "$systemd_evidence"
    if systemctl show-environment >/dev/null 2>&1; then
      refuse "systemd-user-manager-unavailable" "systemd is available, but no user session bus is reachable. Log in normally, or run \`loginctl enable-linger \$USER\` then start a new login session; if it exists, export \`XDG_RUNTIME_DIR=/run/user/\$(id -u)\`."
    fi
    refuse "systemd-unavailable" "Linux setup is unsupported because systemd is not available on this host"
  fi
  rm -f "$systemd_evidence"
fi

if ! command -v curl >/dev/null 2>&1; then
  refuse "download-tool-unavailable" "curl is required to download the release index and archive"
fi
if ! command -v unzip >/dev/null 2>&1; then
  refuse "extraction-failed" "unzip is required to extract the release archive"
fi

index_url="${CK_RELEASE_INDEX_URL:-$INDEX_URL_DEFAULT}"
archive_name="ck-${os}-${arch}.zip"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/cortexkit"
cmd_dir="$data_dir/cmd"
destination="$cmd_dir/ck"
manifest="$data_dir/installer-manifest.json"

if ! temp_dir=$(mktemp -d); then
  refuse "download-failed" "could not create a temporary download directory"
fi
trap cleanup_staging EXIT HUP INT TERM
if ! is_private_staging_directory "$temp_dir"; then
  refuse "download-failed" "temporary download directory is not owner-only or is not owned by this user"
fi
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

ensure_created_directory_tree "$extract_dir" 700 extraction-failed
if ! is_private_staging_directory "$extract_dir"; then
  refuse "extraction-failed" "extraction directory is not owner-only or is not owned by this user"
fi
if ! unzip -q "$archive_path" -d "$extract_dir"; then
  refuse "extraction-failed" "could not extract $archive_name"
fi
candidate="$extract_dir/ck"
if [[ ! -f "$candidate" ]]; then
  refuse "extraction-failed" "$archive_name did not contain ck at its archive root"
fi
if ! chmod 755 "$candidate"; then
  refuse "extraction-failed" "could not set executable mode on extracted ck"
fi
candidate_digest=$(sha256_for "$candidate")

ensure_created_directory_tree "$data_dir" 700
ensure_created_directory_tree "$cmd_dir" 755
if ! is_owned_nonwritable_directory "$data_dir" || ! is_owned_nonwritable_directory "$cmd_dir"; then
  refuse "placement-failed" "data and command directories must be owned by this user and not writable by others; run ck setup to repair existing paths"
fi

if [[ -f "$destination" ]] && cmp -s "$candidate" "$destination"; then
  printf 'ck already matches verified download at %s; skipping placement.\n' "$destination"
else
  # This private sibling keeps the mode change off the destination path and
  # makes the final move a same-filesystem atomic rename.
  if ! placement_stage=$(mktemp -d "$cmd_dir/.ck-stage.XXXXXX"); then
    refuse "placement-failed" "could not prepare destination staging directory"
  fi
  if ! is_private_staging_directory "$placement_stage"; then
    refuse "placement-failed" "destination staging directory is not owner-only or is not owned by this user"
  fi
  placement_tmp="$placement_stage/ck"
  if ! cp "$candidate" "$placement_tmp"; then
    rm -f "$placement_tmp"
    refuse "placement-failed" "could not copy ck to $destination"
  fi
  if ! chmod 755 "$placement_tmp"; then
    rm -f "$placement_tmp"
    refuse "placement-failed" "could not make $destination executable"
  fi
  if ! mv "$placement_tmp" "$destination"; then
    refuse "placement-failed" "could not place ck at $destination"
  fi
  if ! rmdir "$placement_stage"; then
    refuse "placement-failed" "could not remove destination staging directory"
  fi
  placement_stage=""
  printf 'Installed ck at %s.\n' "$destination"
fi

select_profile
# Quote the resolved installation path, rather than assuming the default data
# home. Shell metacharacters in a user-selected directory must remain literal.
profile_cmd_dir=${cmd_dir//\\/\\\\}
profile_cmd_dir=${profile_cmd_dir//\"/\\\"}
profile_cmd_dir=${profile_cmd_dir//\$/\\\$}
if [[ "$profile_kind" == "fish" ]]; then
  path_block="$PATH_BLOCK_BEGIN
set -gx PATH \"$profile_cmd_dir\" \$PATH
$PATH_BLOCK_END"
else
  profile_cmd_dir=${profile_cmd_dir//\`/\\\`}
  path_block="$PATH_BLOCK_BEGIN
export PATH=\"$profile_cmd_dir:\$PATH\"
$PATH_BLOCK_END"
fi
ensure_profile_path "$profile_path" "$path_block"
write_manifest "$manifest" "$destination" "$candidate_digest" "$profile_path" "$expected_digest"

printf 'Next: ck setup\n'
