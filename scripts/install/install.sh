#!/usr/bin/env bash
# Download and verify the CLI archive, then hand installation to ck. Bootstrap
# trusts TLS to the release host; setup verifies its signed release index.
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

select_profile() {
  if [[ -n "${CK_PROFILE_PATH:-}" ]]; then
    profile_path="$CK_PROFILE_PATH"
    profile_kind="posix"
    return
  fi

  case "${SHELL##*/}" in
    zsh)
      profile_path="${ZDOTDIR:-$HOME}/.zshrc"
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

check_profile() {
  local profile="$1"
  if [[ ( -e "$profile" || -L "$profile" ) && ! -f "$profile" ]]; then
    refuse "path-update-failed" "$profile is not a regular file"
  fi
  if [[ -f "$profile" ]] && ! awk -v begin="$PATH_BLOCK_BEGIN" -v end="$PATH_BLOCK_END" '
    $0 == begin { if (inside || seen) { invalid = 1; exit 1 } inside=1; seen=1; next }
    $0 == end { if (!inside) { invalid=1; exit 1 } inside=0 }
    END { if (inside || invalid) exit 1 }
  ' "$profile"; then
    refuse "path-update-failed" "$profile has malformed CortexKit PATH markers"
  fi
}
verify_cli_resolution() {
  local winner
  # The quoted programs expand their arguments in the new shell, not here.
  # shellcheck disable=SC2016
  if [[ "$profile_kind" == fish ]]; then
    winner=$("$SHELL" --no-config --interactive -c 'source "$argv[1]" >&2; command --search ck' "$profile_path" 9>&-) || winner='<not found>'
  elif [[ "${SHELL##*/}" == zsh ]]; then
    winner=$("$SHELL" -f -i -c '. "$1" >&2; whence -p ck' _ "$profile_path" 9>&-) || winner='<not found>'
  else
    winner=$(bash --noprofile --norc -i -c '. "$1" >&2; type -P ck' _ "$profile_path" 9>&-) || winner='<not found>'
  fi
  [[ "$winner" -ef "$destination" ]] || refuse "cli-shadowed" "ck resolves to $winner (PATH entry ${winner%/*}) instead of $destination"
}

cleanup_staging() {
  rm -rf "$temp_dir"
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
    version = json_string(core, "version")
    if (url == "" || sha == "" || version == "") exit 1
    print url
    print sha
    print version
  }
' "$index_path") || refuse "release-incomplete" "ck asset for ${os}-${arch} is missing from $index_url"

archive_url=$(printf '%s\n' "$ck_fields" | sed -n '1p')
expected_digest=$(printf '%s\n' "$ck_fields" | sed -n '2p' | tr 'A-F' 'a-f')
release_version=$(printf '%s\n' "$ck_fields" | sed -n '3p')
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
select_profile
check_profile "$profile_path"
if ! "$candidate" __install-bootstrap "$expected_digest" "$release_version"; then
  refuse "placement-failed" "verified ck could not complete journaled placement"
fi
if ! CK_BOOTSTRAP_PROFILE="$profile_path" CK_BOOTSTRAP_PROFILE_KIND="$profile_kind" "$destination" setup; then
  refuse "setup-failed" "ck setup did not complete; run $destination setup to retry"
fi
verify_cli_resolution
printf 'New terminals find ck on PATH. To load it in this terminal, run:\n  source %q\n' "$profile_path"
