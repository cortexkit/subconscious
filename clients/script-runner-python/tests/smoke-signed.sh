#!/usr/bin/env bash
# Runs a signed ck-script-runner-python end to end. The hardened runtime can stop a binary whose
# signature still verifies, so the signed bytes themselves must start CPython, import the
# OpenSSL- and SQLite-backed modules, and run a script received on the daemon's channel.
#
#   smoke-signed.sh <runner> <python>
#
# <python> runs the shared pipe harness (clients/script-runner/tests/pipe-launcher.py).
set -euo pipefail

binary="${1:?usage: $0 <runner> <python>}"
python="${2:?usage: $0 <runner> <python>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
launcher="$here/../../script-runner/tests/pipe-launcher.py"

# Same convention as macos-sign-notarize.sh: a copy named ckdev-<name>, so a smoke process is
# never listed under a production ck-* name. It lives under /tmp rather than $TMPDIR because
# macOS can take minutes to run a brand-new executable from the per-user $TMPDIR the first time.
smoke_dir="$(mktemp -d /tmp/subc-smoke.XXXXXX)"
finished=0
cleanup() {
  rm -rf "$smoke_dir"
  # Fail closed: any exit before the last line, even one that reports success, is a failure.
  if [[ "$finished" != 1 ]]; then
    echo "signed runner smoke run did not finish" >&2
    exit 1
  fi
}
trap cleanup EXIT
smoke="$smoke_dir/ckdev-script-runner-python"
cp -p "$binary" "$smoke"

version="$(env -i PATH="$PATH" "$smoke" --version)"
script='import hashlib, json, sqlite3, ssl; print("SMOKE_OK", json.dumps([ssl.OPENSSL_VERSION, sqlite3.sqlite_version, hashlib.sha256(b"").hexdigest()[:8]]))'
result="$(
  "$python" -I -c 'import hashlib, struct, sys
script = sys.argv[1].encode()
sys.stdout.buffer.write(struct.pack("<8sIQ32s", b"CKSCRIPT", 1, len(script), hashlib.sha256(script).digest()) + script)' "$script" |
    env -i PATH="$PATH" "$python" -I "$launcher" "$smoke" valid
)"
"$python" -I -c 'import json, sys
result = json.loads(sys.argv[1])
if result["code"] != 0 or not result["out"].startswith("SMOKE_OK") or result["err"]:
    sys.exit(f"signed runner failed its smoke run: {result}")
print(result["out"].strip())' "$result"
echo "signed ck-script-runner-python runs: $version"
finished=1
