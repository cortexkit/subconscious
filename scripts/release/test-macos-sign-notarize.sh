#!/usr/bin/env bash
set -euo pipefail
finished=

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="$(mktemp -d "${SCRIPT_DIR}/.test-release-notarize.XXXXXX")"
finish() {
  local rc=$?
  if [[ "$rc" != 0 ]]; then
    local log
    for log in "$FIXTURE_DIR/"*output; do
      if [[ -f "$log" ]]; then
        printf '\n--- %s ---\n' "$(basename "$log")" >&2
        cat "$log" >&2
      fi
    done
  fi
  rm -rf "$FIXTURE_DIR"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "test-macos-sign-notarize.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

mkdir -p "$FIXTURE_DIR/bin" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist"
for binary in ck ck-subc ck-subc-mcp ck-script-runner-bun ck-script-runner-python ck-nats-server ck-nats-engine ck-bus; do
  # Signing smoke-runs a copied binary with --version before checking identities.
  cat > "$FIXTURE_DIR/source/$binary" <<EOF
#!/bin/sh
if [ '$binary' = ck ] && [ "\${1:-}" = --ck-build-shape ]; then
  echo 'test-support: off'
  exit 0
fi
[ "\${1:-}" = --version ] || exit 64
printf '%s\n' '$binary 1.2.3'
EOF
  chmod +x "$FIXTURE_DIR/source/$binary"
done

cat > "$FIXTURE_DIR/bin/security" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == cms ]]; then
  cat "$4"
  exit 0
fi
printf '  1) %s "Developer ID Application: CortexKit (ABCDE12345)"\n' "$CK_TEST_CERT_SHA1"
EOF

cat > "$FIXTURE_DIR/bin/codesign" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$CK_TEST_CODESIGN_LOG"
if [[ "$1" == "-dv" ]]; then
  binary="${@: -1}"
  if [[ "$binary" == *"/${CK_TEST_MIXED_BINARY:-Subconscious.app}" ]]; then
    printf 'Authority=Developer ID Application: Other Team (OTHER12345)\n' >&2
  else
    printf 'Authority=Developer ID Application: CortexKit (ABCDE12345)\n' >&2
  fi
fi
if [[ "$1" == -d && "${@: -1}" == *'/Subconscious.app' ]]; then
  cat "$CK_TEST_ENTITLEMENTS"
fi
if [[ "$1" == -d && "${@: -1}" == */ck-script-runner-python && "${CK_TEST_PYTHON_ENTITLEMENTS:-}" == 1 ]]; then
  printf '<plist><dict><key>com.apple.security.cs.allow-jit</key><true/></dict></plist>\n'
fi
if [[ "$1" == -d && "${@: -1}" == */ck-script-runner-bun ]]; then
  printf '<plist><dict><key>com.apple.security.cs.allow-jit</key><true/></dict></plist>\n'
fi
EOF

cat > "$FIXTURE_DIR/bin/xcrun" <<EOF
#!/usr/bin/env bash
touch "$FIXTURE_DIR/notarytool-invoked"
printf '%s\n' "\$*" >> "$FIXTURE_DIR/notary.log"
EOF

cat > "$FIXTURE_DIR/bin/vtool" <<'EOF'
#!/bin/sh
printf 'cmd LC_BUILD_VERSION\n platform MACOS\n minos 13.0\n'
EOF
cat > "$FIXTURE_DIR/bin/ditto" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == -c ]]; then
  source_path=$4
  destination=$5
  (cd "$(dirname "$source_path")" && zip -qr "$destination" "$(basename "$source_path")")
else
  cp -R "$1" "$2"
fi
EOF
chmod +x "$FIXTURE_DIR/bin/"*
export MACOS_DAEMON_PROFILE_PATH="$FIXTURE_DIR/profile"
export CK_TEST_CODESIGN_LOG="$FIXTURE_DIR/codesign.log"
export CK_TEST_ENTITLEMENTS="$SCRIPT_DIR/macos/daemon.entitlements"
CK_TEST_CERT_SHA1=$(python3 - "$MACOS_DAEMON_PROFILE_PATH" <<'PY'
import hashlib
import plistlib
import sys
from pathlib import Path

certificate = b"authorized certificate"
Path(sys.argv[1]).write_bytes(plistlib.dumps({
    "Entitlements": {"com.apple.application-identifier": "5R5846NBPW.io.cortexkit.daemon"},
    "DeveloperCertificates": [certificate],
}))
print(hashlib.sha1(certificate).hexdigest().upper())
PY
)
export CK_TEST_CERT_SHA1

set +e
PATH="$FIXTURE_DIR/bin:$PATH" \
  "$SCRIPT_DIR/macos-sign-notarize.sh" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist" \
  > "$FIXTURE_DIR/output" 2>&1
status=$?
set -e

if [[ "$status" -eq 0 ]]; then
  echo "mixed-identity fixture unexpectedly succeeded" >&2
  exit 1
fi
grep -Fq "refusing mixed signing identities before notarization" "$FIXTURE_DIR/output"
if [[ -e "$FIXTURE_DIR/notarytool-invoked" ]]; then
  echo "mixed-identity fixture reached notarization" >&2
  exit 1
fi

rm -rf "$FIXTURE_DIR/source/Subconscious.app"
if PATH="$FIXTURE_DIR/bin:$PATH" CK_TEST_MIXED_BINARY=none CK_TEST_PYTHON_ENTITLEMENTS=1 \
  "$SCRIPT_DIR/macos-sign-notarize.sh" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist" \
  > "$FIXTURE_DIR/python-output" 2>&1; then
  echo 'Python runner with JIT entitlement unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'refusing Python runner: it carries entitlements:' "$FIXTURE_DIR/python-output"
[[ ! -e "$FIXTURE_DIR/notarytool-invoked" ]]
rm -rf "$FIXTURE_DIR/source/Subconscious.app"
# The daemon smoke run must still be inside a complete renamed bundle.
cat > "$FIXTURE_DIR/source/ck-subc" <<'EOF'
#!/bin/sh
[ "$1" = --version ] || exit 64
case "$0" in
  */ckdev-subc.app/Contents/MacOS/ck-subc)
    [ -f "$(dirname "$0")/../embedded.provisionprofile" ] || exit 65
    ;;
  */source/ck-subc|*'/Subconscious.app/Contents/MacOS/ck-subc') ;;
  *) exit 66 ;;
esac
echo 'ck-subc 1.2.3'
EOF
chmod +x "$FIXTURE_DIR/source/ck-subc"
PATH="$FIXTURE_DIR/bin:$PATH" CK_TEST_MIXED_BINARY=none \
  APP_STORE_CONNECT_API_KEY_PATH="$FIXTURE_DIR/profile" \
  APP_STORE_CONNECT_API_KEY_ID=fixture APP_STORE_CONNECT_API_ISSUER_ID=fixture \
  "$SCRIPT_DIR/macos-sign-notarize.sh" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist" \
  > "$FIXTURE_DIR/success-output" 2>&1
grep -Fq "stapler staple $FIXTURE_DIR/source/Subconscious.app" "$FIXTURE_DIR/notary.log"
grep -Fq "stapler validate $FIXTURE_DIR/source/Subconscious.app" "$FIXTURE_DIR/notary.log"
if grep -Fq 'stapler staple '"$FIXTURE_DIR/dist/ck-subc-darwin-arm64.zip" "$FIXTURE_DIR/notary.log"; then
  echo 'daemon transport zip was stapled instead of the bundle' >&2; exit 1
fi
unzip -Z1 "$FIXTURE_DIR/dist/ck-subc-darwin-arm64.zip" | grep -Fxq 'Subconscious.app/Contents/embedded.provisionprofile'
(cd "$FIXTURE_DIR/dist" && shasum -a 256 -c ck-subc-darwin-arm64.zip.sha256)
for binary in ck-nats-server ck-nats-engine ck-bus; do
  grep -Fq -- "--force --options runtime --timestamp --sign Developer ID Application: CortexKit (ABCDE12345) $FIXTURE_DIR/source/$binary" "$FIXTURE_DIR/codesign.log"
  grep -Fq "notarytool submit $FIXTURE_DIR/dist/$binary-darwin-arm64.zip" "$FIXTURE_DIR/notary.log"
  [[ "$(unzip -Z1 "$FIXTURE_DIR/dist/$binary-darwin-arm64.zip")" == "$binary" ]]
  (cd "$FIXTURE_DIR/dist" && shasum -a 256 -c "$binary-darwin-arm64.zip.sha256")
done
printf 'signing fixtures passed (6 checks: mixed identity, Python entitlements, bundled notarization, NATS wrapper, NATS engine, ck-bus)\n'
bash "$SCRIPT_DIR/test-macos-daemon-bundle.sh"
finished=1
