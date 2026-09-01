#!/usr/bin/env bash
# Sign, notarize, and package the darwin-arm64 subconscious release archives.
set -euo pipefail

SOURCE_DIR="${1:?usage: $0 <source-dir> <dist-dir>}"
DIST_DIR="${2:?usage: $0 <source-dir> <dist-dir>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BINARIES=(ck ck-subc ck-subc-mcp)

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

for binary in "${BINARIES[@]}"; do
  binary_path="${SOURCE_DIR}/${binary}"
  [[ -f "$binary_path" ]] || fail "missing release binary: ${binary_path}"

  codesign --force --options runtime --timestamp --sign "$identity" "$binary_path"
  codesign --verify --strict --verbose=2 "$binary_path"

  # Execute the signed binary: the hardened runtime can break a binary whose
  # signature still verifies perfectly, and nothing else runs the bytes that
  # actually ship.
  version_output="$("$binary_path" --version)" || fail "signed binary refuses to run: ${binary}"
  [[ -n "$version_output" ]] || fail "signed binary produced no --version output: ${binary}"
  echo "signed ${binary} runs: ${version_output}"

  signed_identity="$(
    codesign -dv --verbose=4 "$binary_path" 2>&1 |
      awk -F= '$1 == "Authority" && $2 ~ /^Developer ID Application:/ { print $2; exit }'
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

for binary in "${BINARIES[@]}"; do
  archive="${DIST_DIR}/${binary}-darwin-arm64.zip"
  xcrun notarytool submit "$archive" \
    --key "$APP_STORE_CONNECT_API_KEY_PATH" \
    --key-id "$APP_STORE_CONNECT_API_KEY_ID" \
    --issuer "$APP_STORE_CONNECT_API_ISSUER_ID" \
    --wait

  # Zip assets cannot always receive a ticket immediately. Submission already
  # completed successfully, so an unavailable staple is recorded but does not
  # block the allowed notarized-and-unstapled alpha distribution.
  if ! xcrun stapler staple "$archive"; then
    record_staple_pending "$archive"
  fi

  # Recompute the digest sidecar from the bytes that will actually publish:
  # stapling mutates the archive when it succeeds, and a sidecar computed at
  # package time would then describe bytes nobody receives.
  (cd "$DIST_DIR" && shasum -a 256 "$(basename "$archive")" > "$(basename "$archive").sha256")
done
