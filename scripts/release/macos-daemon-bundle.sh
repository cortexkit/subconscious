#!/usr/bin/env bash
# Bundle the daemon's profile so macOS can authorize its entitlements at launch.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENTITLEMENTS="$SCRIPT_DIR/macos/daemon.entitlements"
usage() {
  cat <<'HELP'
Usage: macos-daemon-bundle.sh <ck-subc> <profile> <signing identity> <out dir>
       macos-daemon-bundle.sh --unentitled <ck-subc> <out dir>

Build Subconscious.app, sign with hardened runtime and a timestamp, and
verify its signature and exact entitlements. The profile must authorize
5R5846NBPW.io.cortexkit.daemon. The output bundle must not already exist.

--unentitled creates an ad hoc signed alpha/test bundle without a provisioning
profile or entitlements. It cannot access the daemon's keychain group; an
unstamped install runs degraded. Identity '-' is only used by this mode.
HELP
}
fail() { echo "$*" >&2; exit 1; }

unentitled=0
profile=
if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
  usage
  exit 0
elif [[ "${1:-}" == --unentitled ]]; then
  [[ $# == 3 ]] || { usage >&2; exit 2; }
  unentitled=1
  binary=$2
  identity=-
  out_dir=$3
else
  [[ $# == 4 ]] || { usage >&2; exit 2; }
  binary=$1
  profile=$2
  identity=$3
  out_dir=$4
fi
[[ -x "$binary" && -f "$binary" ]] || fail "daemon binary is not executable: $binary"
bundle="$out_dir/Subconscious.app"
[[ ! -e "$bundle" ]] || fail "refusing to overwrite existing bundle: $bundle"
created_out=0
if [[ ! -d "$out_dir" ]]; then created_out=1; fi
mkdir -p "$out_dir"
scratch=$(mktemp -d "$out_dir/.daemon-bundle.XXXXXX")
finished=
cleanup() {
  local rc=$?
  rm -rf "$scratch"
  if [[ "${finished:-}" != 1 ]]; then
    rm -rf "$bundle"
    if [[ "$created_out" == 1 ]]; then rmdir "$out_dir"; fi
    if [[ "$rc" == 0 ]]; then
      echo 'daemon bundle build exited before finishing' >&2
      rc=1
    fi
  fi
  exit "$rc"
}
trap cleanup EXIT

if [[ "$unentitled" == 0 ]]; then
  security cms -D -i "$profile" > "$scratch/profile.plist"
  python3 - "$scratch/profile.plist" <<'PY'
import plistlib
import sys

with open(sys.argv[1], "rb") as source:
    profile = plistlib.load(source)
entitlements = profile.get("Entitlements", {})
actual = entitlements.get("com.apple.application-identifier", entitlements.get("application-identifier"))
if actual != "5R5846NBPW.io.cortexkit.daemon":
    sys.exit(f"refusing daemon profile: application-identifier is {actual!r}")
if "application-identifier" in entitlements and entitlements["application-identifier"] != actual:
    sys.exit("refusing daemon profile: conflicting application-identifier keys")
PY
  [[ "$identity" != - && -n "$identity" ]] || fail "use --unentitled for an ad hoc bundle"
  security find-identity -v -p codesigning > "$scratch/identities"
  identity=$(python3 - "$scratch/profile.plist" "$scratch/identities" "$identity" <<'PY'
import hashlib
import plistlib
import re
import sys
from pathlib import Path

profile = plistlib.loads(Path(sys.argv[1]).read_bytes())
identities = re.findall(r'^\s*\d+\)\s+([A-Fa-f0-9]{40})\s+"([^"\n]+)"', Path(sys.argv[2]).read_text(), re.M)
matches = {fingerprint.upper() for fingerprint, name in identities if name == sys.argv[3] or fingerprint.upper() == sys.argv[3].upper()}
if len(matches) != 1:
    sys.exit("refusing daemon signer: select exactly one valid signing identity by name or SHA-1")
fingerprint = matches.pop()
authorized = {hashlib.sha1(cert).hexdigest().upper() for cert in profile.get("DeveloperCertificates", [])}
if fingerprint not in authorized:
    sys.exit("refusing daemon signer: signing certificate is not authorized by the daemon profile")
print(fingerprint)
PY
  )
fi

version_output=$("$binary" --version)
vtool -show-build "$binary" > "$scratch/build-version"
mkdir -p "$bundle/Contents/MacOS"
cp -p "$binary" "$bundle/Contents/MacOS/ck-subc"
python3 - "$version_output" "$scratch/build-version" "$bundle/Contents/Info.plist" <<'PY'
import plistlib
import re
import sys
from pathlib import Path

match = re.fullmatch(r"ck-subc (\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.+-]+)?)", sys.argv[1])
if not match:
    sys.exit(f"unrecognized ck-subc --version output: {sys.argv[1]!r}")
minimums = re.findall(r"^\s*minos (\d+(?:\.\d+){1,2})\s*$", Path(sys.argv[2]).read_text(), re.M)
if not minimums:
    sys.exit("vtool found no LC_BUILD_VERSION minos for the daemon")
minimum = max(minimums, key=lambda value: tuple(map(int, value.split("."))))
info = {
    "CFBundleIdentifier": "io.cortexkit.daemon",
    "CFBundleExecutable": "ck-subc",
    "CFBundleName": "Subconscious",
    "CFBundleDisplayName": "Subconscious",
    "CFBundlePackageType": "APPL",
    "CFBundleVersion": match[1],
    "CFBundleShortVersionString": match[1],
    "LSUIElement": True,
    "LSMinimumSystemVersion": minimum,
}
with open(sys.argv[3], "wb") as dest:
    plistlib.dump(info, dest)
PY

sign_args=(--force --options runtime --sign "$identity")
if [[ "$unentitled" == 0 ]]; then
  cp -p "$profile" "$bundle/Contents/embedded.provisionprofile"
  sign_args+=(--timestamp --entitlements "$ENTITLEMENTS")
else
  # Ad hoc signatures cannot obtain a secure signing timestamp.
  sign_args+=(--timestamp=none)
fi
codesign "${sign_args[@]}" "$bundle" # nonempty-array: literal signing options
codesign --verify --strict --deep "$bundle"
codesign -d --entitlements :- "$bundle" > "$scratch/signed-entitlements.plist" 2>/dev/null
python3 - "$ENTITLEMENTS" "$scratch/signed-entitlements.plist" "$unentitled" <<'PY'
import plistlib
import sys
from pathlib import Path

expected = {} if sys.argv[3] == "1" else plistlib.loads(Path(sys.argv[1]).read_bytes())
signed = Path(sys.argv[2]).read_bytes()
actual = plistlib.loads(signed) if signed.strip() else {}
if actual != expected:
    sys.exit(f"refusing daemon bundle: signed entitlements differ: {actual!r}")
PY
# Signature verification alone does not check whether macOS will permit launch.
signed_version=$("$bundle/Contents/MacOS/ck-subc" --version) || fail "bundled daemon refuses to run"
[[ "$signed_version" == "$version_output" ]] || fail "bundled daemon version differs after signing"
printf 'Built and verified %s\n' "$bundle"
finished=1
