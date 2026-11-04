#!/usr/bin/env bash
# A module that re-executes its own binary path while it runs must not have
# that binary swapped without a restart, so place-module.sh refuses
# --no-restart for it unless --reexec-safe says its owner confirmed otherwise.
# Other modules are unaffected. Each case stops before touching anything: the
# staged path doesn't exist, so a case that gets past the guard ends on the
# staged-artifact refusal instead.
set -u
finished=
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
PLACE_SCRIPT="$ROOT/scripts/fleet/place-module.sh"
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/place-module-reexec.XXXXXX") || exit 1
cleanup() {
  local rc=$?
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "place-module-reexec.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap cleanup EXIT
MISSING="$TEST_ROOT/missing"
mkdir -p "$TEST_ROOT/bin" || exit 1
# Stub supervisor status checks so restart cases never contact the operator's daemon.
cat >"$TEST_ROOT/bin/ck" <<'EOF' || exit 1
#!/usr/bin/env bash
case "$*" in
  'module status cerebellum'|'module status basal'|'module status thalamus-harness'|'module status aft') exit 0 ;;
  *) exit 64 ;;
esac
EOF
chmod +x "$TEST_ROOT/bin/ck" || exit 1
export PATH="$TEST_ROOT/bin:$PATH"
failures=0

run_case() {
  local name=$1 expect=$2
  shift 2
  local output
  output=$(bash "$PLACE_SCRIPT" --staged "$MISSING" --marker x --tool-defs none "$@" 2>&1)
  if printf '%s\n' "$output" | grep -qF "$expect"; then
    echo "PASS: $name"
  else
    echo "FAIL: $name: expected '$expect', got: $output"
    failures=$((failures + 1))
  fi
}

run_case reexec-module-refuses-no-restart \
  "REFUSED: --no-restart: cerebellum re-executes its own binary path" \
  --module cerebellum --no-restart
run_case reexec-safe-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module cerebellum --no-restart --reexec-safe
run_case reexec-module-with-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module cerebellum
run_case other-module-no-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module basal --no-restart
# The harness and aft pin their own executables, so they are no longer listed.
run_case pinned-harness-no-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module thalamus-harness --no-restart
run_case pinned-aft-no-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module aft --no-restart

[ "$failures" -eq 0 ] || exit 1
finished=1
