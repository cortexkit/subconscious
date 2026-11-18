#!/usr/bin/env bash
# Sign, notarize, and package the darwin-arm64 subconscious release archives.
set -euo pipefail

SOURCE_DIR="${1:?usage: $0 <source-dir> <dist-dir>}"
DIST_DIR="${2:?usage: $0 <source-dir> <dist-dir>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARIES=(ck ck-subc ck-subc-mcp ck-script-runner-bun ck-script-runner-python ck-nats-server ck-nats-engine)

fail() {
  echo "$*" >&2
  exit 1
}

all_identities="$(security find-identity -v -p codesigning)"
developer_id_identities="$(printf '%s\n' "$all_identities" | awk -F'"' '$2 ~ /^Developer ID Application:/ { print $2 }')"
if [[ -z "$developer_id_identities" ]]; then
  if printf '%s\n' "$all_identities" | grep -Fq '"Apple Development:'; then
    fail "refusing release: only an Apple Development identity is available; a Developer ID Application identity is required"
  fi
  fail "refusing release: no Developer ID Application signing identity is available"
fi

if [[ -n "${DEVELOPER_ID_APPLICATION_IDENTITY:-}" ]]; then
  if [[ "$DEVELOPER_ID_APPLICATION_IDENTITY" != "Developer ID Application:"* ]]; then
    fail "refusing release: configured signing identity is not a Developer ID Application identity"
  fi
  if ! printf '%s\n' "$developer_id_identities" | grep -Fqx -- "$DEVELOPER_ID_APPLICATION_IDENTITY"; then
    fail "refusing release: configured Developer ID Application identity is not available in the keychain"
  fi
  identity="$DEVELOPER_ID_APPLICATION_IDENTITY"
else
  identity_count="$(printf '%s\n' "$developer_id_identities" | awk 'NF { count += 1 } END { print count + 0 }')"
  if [[ "$identity_count" != "1" ]]; then
    fail "refusing release: set DEVELOPER_ID_APPLICATION_IDENTITY to choose one Developer ID Application identity"
  fi
  identity="$(printf '%s\n' "$developer_id_identities" | head -n 1)"
fi

# The embedded helper must be signed by this identity before Bun compiles it.
if [[ "${3:-}" == --print-identity ]]; then
  printf '%s\n' "$identity"
  exit 0
fi

: "${MACOS_DAEMON_PROFILE_PATH:?Daemon provisioning profile path is required}"

for binary in "${BINARIES[@]}"; do # nonempty-array: literal release binaries
  binary_path="${SOURCE_DIR}/${binary}"
  [[ -f "$binary_path" ]] || fail "missing release binary: ${binary_path}"

  if [[ "$binary" == ck-subc ]]; then
    "$SCRIPT_DIR/macos-daemon-bundle.sh" "$binary_path" "$MACOS_DAEMON_PROFILE_PATH" "$identity" "$SOURCE_DIR"
    binary_path="$SOURCE_DIR/Subconscious.app"
  elif [[ "$binary" == ck-script-runner-bun ]]; then
    codesign --force --options runtime --timestamp --entitlements "$SCRIPT_DIR/../../clients/script-runner/entitlements.plist" --sign "$identity" "$binary_path"
    codesign -d --entitlements :- "$binary_path" 2>/dev/null | python3 "$SCRIPT_DIR/check-runner-entitlements.py"
  else
    codesign --force --options runtime --timestamp --sign "$identity" "$binary_path"
    if [[ "$binary" == ck-script-runner-python ]]; then
      entitlements="$(codesign -d --entitlements :- "$binary_path" 2>/dev/null)"
      [[ -z "$entitlements" ]] || fail "refusing Python runner: it carries entitlements: $entitlements"
    fi
  fi
  codesign --verify --strict --verbose=2 "$binary_path"

  # Execute the signed binary: the hardened runtime can break a binary whose
  # signature still verifies perfectly, and nothing else runs the bytes that
  # actually ship. Use a separate ckdev-<name> path (a renamed bundle for the
  # daemon) to distinguish smoke runs from installed production executables.
  # A copy, not a hard link: macOS caches one executable path for each inode.
  smoke_name="ckdev-${binary#ck-}"
  # Under /tmp, not $TMPDIR: a fresh executable under the per-user TMPDIR can
  # stall its first system-policy evaluation for many minutes on macOS.
  smoke_dir="$(mktemp -d /tmp/subc-smoke.XXXXXX)"
  smoke_path="${smoke_dir}/${smoke_name}"
  if [[ "$binary" == ck-subc ]]; then
    # Restricted entitlements require the adjacent embedded profile, so run
    # the executable inside a renamed copy of the entire signed bundle.
    ditto "$binary_path" "$smoke_path.app"
    smoke_path="$smoke_path.app/Contents/MacOS/ck-subc"
  else
    cp -p "$binary_path" "$smoke_path"
  fi
  version_output="$("$smoke_path" --version)" || { rm -rf "$smoke_dir"; fail "signed binary refuses to run: ${binary}"; }
  rm -rf "$smoke_dir"
  [[ -n "$version_output" ]] || fail "signed binary produced no --version output: ${binary}"
  echo "signed ${binary} runs: ${version_output}"

  # awk must consume codesign's whole output: an early `exit` on the first
  # match leaves codesign writing into a closed pipe, and under pipefail that
  # SIGPIPE (141) aborts the release before notarization, timing-dependently.
  signed_identity="$(
    codesign -dv --verbose=4 "$binary_path" 2>&1 |
      awk -F= '$1 == "Authority" && $2 ~ /^Developer ID Application:/ && !found { print $2; found = 1 }'
  )"
  if [[ "$signed_identity" != "$identity" ]]; then
    fail "refusing mixed signing identities before notarization: ${binary} has '${signed_identity:-no Developer ID Application authority}', expected '${identity}'"
  fi
done

: "${APP_STORE_CONNECT_API_KEY_PATH:?App Store Connect API key path is required}"
: "${APP_STORE_CONNECT_API_KEY_ID:?App Store Connect API key ID is required}"
: "${APP_STORE_CONNECT_API_ISSUER_ID:?App Store Connect API issuer ID is required}"
[[ -r "$APP_STORE_CONNECT_API_KEY_PATH" ]] || fail "App Store Connect API key is not readable: ${APP_STORE_CONNECT_API_KEY_PATH}"

"${SCRIPT_DIR}/package-unix-archives.sh" darwin arm64 "$SOURCE_DIR" "$DIST_DIR"

record_staple_pending() {
  local archive="$1"
  local message="Notarization completed for ${archive}, but stapling did not complete. The notarized archive is published unstapled for alpha distribution."

  echo "::warning title=staple-pending::${message}"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    {
      echo "## staple-pending"
      echo
      echo "$message"
    } >> "$GITHUB_STEP_SUMMARY"
  else
    echo "staple-pending: ${message}" >&2
  fi
}

for binary in "${BINARIES[@]}"; do # nonempty-array: literal release binaries
  archive="${DIST_DIR}/${binary}-darwin-arm64.zip"
  xcrun notarytool submit "$archive" \
    --key "$APP_STORE_CONNECT_API_KEY_PATH" \
    --key-id "$APP_STORE_CONNECT_API_KEY_ID" \
    --issuer "$APP_STORE_CONNECT_API_ISSUER_ID" \
    --wait

  if [[ "$binary" == ck-subc ]]; then
    # Staple the app, not its transport zip, then publish the stapled bytes.
    xcrun stapler staple "$SOURCE_DIR/Subconscious.app"
    xcrun stapler validate "$SOURCE_DIR/Subconscious.app"
    rm -f "$archive"
    ditto -c -k --keepParent "$SOURCE_DIR/Subconscious.app" "$archive"
  # Other binaries keep their existing notarized-but-unstapled fallback:
  # transport zips cannot always receive a ticket immediately.
  elif ! xcrun stapler staple "$archive"; then
    record_staple_pending "$archive"
  fi

  # Recompute the digest sidecar from the bytes that will actually publish:
  # stapling mutates the archive when it succeeds, and a sidecar computed at
  # package time would then describe bytes nobody receives.
  (cd "$DIST_DIR" && shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256")
done
