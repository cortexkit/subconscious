#!/usr/bin/env bash
# Run subc-daemon's library tests with their helper programs in place.
#
# Several daemon tests start helper programs (fake-aft-stub,
# ckdev-resource-helper, the privacy trampoline fixture and others) and find
# them by walking up from the test binary. `cargo test` builds the test binary
# but not those helpers, and a separate `cargo build` may not put them where
# the walk looks, so a plain `cargo test -p subc-daemon --lib` fails dozens of
# tests on a fresh build-server checkout for reasons unrelated to the code.
# This script builds the helpers and the test binary in one go, copies every
# helper next to the test binary, then runs it.
#
# Usage, from the repository root:
#   scripts/test-daemon-lib.sh [--features LIST] [-- TEST-BINARY-ARGS...]
# Default features: test-support (add dev-desktop-approval on branches that
# have it). Everything after `--` goes to the test binary, for example
#   scripts/test-daemon-lib.sh -- --exact supervise::tests::some_test
#   scripts/test-daemon-lib.sh -- --test-threads=8
# On macOS, use a target directory on the internal disk: native fixtures
# refuse to run from an external volume.
set -euo pipefail
FEATURES=test-support
while [ $# -gt 0 ]; do
  case "$1" in
    --features) FEATURES=$2; shift 2 ;;
    --) shift; break ;;
    *) echo "unknown argument: $1 (test-binary arguments go after --)" >&2; exit 64 ;;
  esac
done
# Print "<target name> <is test> <executable>" for every built artifact.
artifacts() {
  python3 -c 'import json,sys
for l in sys.stdin:
    try: m=json.loads(l)
    except Exception: continue
    if m.get("reason")=="compiler-artifact" and m.get("executable"):
        print(m["target"]["name"], m["profile"]["test"], m["executable"])'
}
helpers=$(cargo build --locked -p subc-core -p subc-daemon -p subc-os --bins \
  --features subc-core/test-support,subc-daemon/test-support --message-format=json | artifacts)
test_binary=$(cargo test --locked -p subc-daemon --lib --features "$FEATURES" --no-run \
  --message-format=json | artifacts | awk '$1=="subc_daemon" && $2=="True" {print $3}' | tail -1)
[ -n "$test_binary" ] || { echo "the subc-daemon library test binary was not built" >&2; exit 2; }
deps=$(dirname "$test_binary")
# The walk from the test binary checks its own folder (deps/) and the one above.
printf '%s\n' "$helpers" | awk '{print $3}' | while read -r helper; do
  [ -n "$helper" ] || continue
  for dir in "$deps" "$(dirname "$deps")"; do
    # Skip a destination that already is this file: cargo hard-links built
    # binaries between the two folders, and Windows paths may be spelled
    # differently, so compare files rather than path strings.
    [ "$helper" -ef "$dir/$(basename "$helper")" ] || cp "$helper" "$dir/"
  done
done
exec "$test_binary" "$@"
