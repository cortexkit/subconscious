#!/usr/bin/env bash
# Portable contract checks; native codesign/ditto checks run separately on macOS.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR=$(mktemp -d "$SCRIPT_DIR/.test-daemon-bundle.XXXXXX")
finished=
finish() {
  local rc=$?
  rm -rf "$FIXTURE_DIR"
  if [[ "$rc" == 0 && "${finished:-}" != 1 ]]; then
    echo 'daemon bundle fixtures exited before finishing' >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT
mkdir -p "$FIXTURE_DIR/bin"
export CK_TEST_BUNDLE_LOG="$FIXTURE_DIR/sign.log"
export CK_TEST_ENTITLEMENTS="$SCRIPT_DIR/macos/daemon.entitlements"

cat > "$FIXTURE_DIR/ck-subc" <<'EOF'
#!/bin/sh
[ "$1" = --version ] || exit 64
case "$0" in *.app/Contents/MacOS/ck-subc) [ "${CK_TEST_SMOKE_FAILURE:-}" != 1 ] || exit 66;; esac
echo 'ck-subc 1.2.3'
EOF
cat > "$FIXTURE_DIR/bin/security" <<'EOF'
#!/bin/sh
if [ "$1" = cms ]; then
  cat "$4"
else
  printf '  1) %s "Developer ID fixture"\n' "$CK_TEST_CERT_SHA1"
fi
EOF
cat > "$FIXTURE_DIR/bin/vtool" <<'EOF'
#!/bin/sh
[ "${CK_TEST_NO_BUILD_VERSION:-}" != 1 ] || exit 0
printf 'cmd LC_BUILD_VERSION\n platform MACOS\n minos 13.4\n'
EOF
cat > "$FIXTURE_DIR/bin/codesign" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$CK_TEST_BUNDLE_LOG"
if [ "$1" = -d ] && [ "${CK_TEST_UNENTITLED:-}" != 1 ]; then
  if [ "${CK_TEST_WRONG_ENTITLEMENTS:-}" = 1 ]; then
    printf '<plist><dict><key>unexpected</key><true/></dict></plist>\n'
  else
    cat "$CK_TEST_ENTITLEMENTS"
  fi
fi
EOF
chmod +x "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/bin/"*
export PATH="$FIXTURE_DIR/bin:$PATH"
python3 - "$FIXTURE_DIR" <<'PY'
import plistlib
import hashlib
import sys
from pathlib import Path

certificate = b"authorized certificate"
root = Path(sys.argv[1])
(root / "certificate-sha1").write_text(hashlib.sha1(certificate).hexdigest().upper())
for name, app_id in [("profile", "5R5846NBPW.io.cortexkit.daemon"), ("wrong-profile", "5R5846NBPW.io.example.other")]:
    (root / name).write_bytes(plistlib.dumps({"Entitlements": {"com.apple.application-identifier": app_id}, "DeveloperCertificates": [certificate]}))
(root / "wrong-certificate").write_bytes(plistlib.dumps({"Entitlements": {"com.apple.application-identifier": "5R5846NBPW.io.cortexkit.daemon"}, "DeveloperCertificates": [b"other certificate"]}))
(root / "alternate-key").write_bytes(plistlib.dumps({"Entitlements": {"application-identifier": "5R5846NBPW.io.example.other"}, "DeveloperCertificates": [certificate]}))
PY
CK_TEST_CERT_SHA1=$(< "$FIXTURE_DIR/certificate-sha1")
export CK_TEST_CERT_SHA1

test_certificate_mismatch() {
  if "$SCRIPT_DIR/macos-daemon-bundle.sh" "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/wrong-certificate" 'Developer ID fixture' "$FIXTURE_DIR/cert-mismatch" > "$FIXTURE_DIR/error" 2>&1; then
    echo 'FAIL certificate_mismatch: signer not covered by profile was accepted' >&2; return 1
  fi
  grep -Fq 'signing certificate is not authorized by the daemon profile' "$FIXTURE_DIR/error"
  [[ ! -e "$FIXTURE_DIR/cert-mismatch" && ! -e "$CK_TEST_BUNDLE_LOG" ]]
  echo 'PASS certificate_mismatch'
}
test_profile_refusal_cleanup() {
  if "$SCRIPT_DIR/macos-daemon-bundle.sh" "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/wrong-profile" 'Developer ID fixture' "$FIXTURE_DIR/refused-output" > "$FIXTURE_DIR/error" 2>&1; then
    echo 'FAIL profile_refusal_cleanup: wrong profile was accepted' >&2; return 1
  fi
  if [[ -e "$FIXTURE_DIR/refused-output" ]]; then
    echo 'FAIL profile_refusal_cleanup: refused run left its output directory' >&2; return 1
  fi
  echo 'PASS profile_refusal_cleanup'
}
if [[ -n "${1:-}" ]]; then
  case "$1" in
    certificate_mismatch) test_certificate_mismatch ;;
    profile_refusal_cleanup) test_profile_refusal_cleanup ;;
    *) echo "unknown test: $1" >&2; exit 2 ;;
  esac
  finished=1
  exit 0
fi
test_certificate_mismatch
test_profile_refusal_cleanup

"$SCRIPT_DIR/macos-daemon-bundle.sh" "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/profile" 'Developer ID fixture' "$FIXTURE_DIR/good"
python3 - "$FIXTURE_DIR" <<'PY'
import plistlib
import sys
from pathlib import Path

root = Path(sys.argv[1])
contents = root / "good/CortexKit Daemon.app/Contents"
assert (contents / "MacOS/ck-subc").read_bytes() == (root / "ck-subc").read_bytes()
assert (contents / "embedded.provisionprofile").read_bytes() == (root / "profile").read_bytes()
assert plistlib.loads((contents / "Info.plist").read_bytes()) == {
    "CFBundleIdentifier": "io.cortexkit.daemon",
    "CFBundleExecutable": "ck-subc",
    "CFBundleName": "CortexKit Daemon",
    "CFBundlePackageType": "APPL",
    "CFBundleVersion": "1.2.3",
    "CFBundleShortVersionString": "1.2.3",
    "LSUIElement": True,
    "LSMinimumSystemVersion": "13.4",
}
log = (root / "sign.log").read_text()
assert "--options runtime" in log and "--timestamp --entitlements" in log
assert "--verify --strict --deep" in log
PY
rm "$CK_TEST_BUNDLE_LOG"
if "$SCRIPT_DIR/macos-daemon-bundle.sh" "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/wrong-profile" 'Developer ID fixture' "$FIXTURE_DIR/wrong" > "$FIXTURE_DIR/error" 2>&1; then
  echo 'wrong application-identifier profile was accepted' >&2; exit 1
fi
grep -Fq 'refusing daemon profile: application-identifier' "$FIXTURE_DIR/error"
[[ ! -e "$CK_TEST_BUNDLE_LOG" && ! -e "$FIXTURE_DIR/wrong/CortexKit Daemon.app" ]]

if CK_TEST_WRONG_ENTITLEMENTS=1 "$SCRIPT_DIR/macos-daemon-bundle.sh" "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/profile" 'Developer ID fixture' "$FIXTURE_DIR/mismatch" > "$FIXTURE_DIR/error" 2>&1; then
  echo 'mismatched signed entitlements were accepted' >&2; exit 1
fi
grep -Fq 'signed entitlements differ' "$FIXTURE_DIR/error"
[[ ! -e "$FIXTURE_DIR/mismatch/CortexKit Daemon.app" ]]

if CK_TEST_NO_BUILD_VERSION=1 "$SCRIPT_DIR/macos-daemon-bundle.sh" --unentitled "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/no-build" > "$FIXTURE_DIR/error" 2>&1; then
  echo 'missing Mach-O build version was accepted' >&2; exit 1
fi
grep -Fq 'vtool found no LC_BUILD_VERSION' "$FIXTURE_DIR/error"
[[ ! -e "$FIXTURE_DIR/no-build/CortexKit Daemon.app" ]]

CK_TEST_UNENTITLED=1 "$SCRIPT_DIR/macos-daemon-bundle.sh" --unentitled "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/alpha"
[[ ! -e "$FIXTURE_DIR/alpha/CortexKit Daemon.app/Contents/embedded.provisionprofile" ]]
[[ -f "$FIXTURE_DIR/alpha/CortexKit Daemon.app/Contents/MacOS/ck-subc" ]]
if "$SCRIPT_DIR/macos-daemon-bundle.sh" "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/alternate-key" 'Developer ID fixture' "$FIXTURE_DIR/alternate-output" > "$FIXTURE_DIR/error" 2>&1; then
  echo 'wrong application-identifier alias profile was accepted' >&2; exit 1
fi
grep -Fq "application-identifier is '5R5846NBPW.io.example.other'" "$FIXTURE_DIR/error"
[[ ! -e "$FIXTURE_DIR/alternate-output" ]]
if CK_TEST_SMOKE_FAILURE=1 CK_TEST_UNENTITLED=1 "$SCRIPT_DIR/macos-daemon-bundle.sh" --unentitled "$FIXTURE_DIR/ck-subc" "$FIXTURE_DIR/smoke-failed" > "$FIXTURE_DIR/error" 2>&1; then
  echo 'bundle whose executable refuses launch was accepted' >&2; exit 1
fi
grep -Fq 'bundled daemon refuses to run' "$FIXTURE_DIR/error"
[[ ! -e "$FIXTURE_DIR/smoke-failed" ]]
printf 'Daemon bundle fixtures passed (9 checks: structure, profile, entitlements, minos, alpha, certificate, cleanup, app-id alias, smoke)\n'
finished=1
