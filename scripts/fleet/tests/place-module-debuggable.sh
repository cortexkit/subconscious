#!/usr/bin/env bash
# place-module.sh lets a get-task-allow build through only with
# --allow-debuggable <ask_id>, never for a first install or a rollback and
# never without the restart that bounds the window. Each case stops before
# touching anything: the staged path doesn't exist, so a case that gets past
# the guard ends on the staged-artifact refusal instead.
set -u
finished=
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
PLACE_SCRIPT="$ROOT/scripts/fleet/place-module.sh"
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/place-module-debuggable.XXXXXX") || exit 1
cleanup() {
  local rc=$?
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "place-module-debuggable.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap cleanup EXIT
MISSING="$TEST_ROOT/missing"
mkdir -p "$TEST_ROOT/bin" || exit 1
# Stub supervisor status checks so no case contacts the operator's daemon.
cat >"$TEST_ROOT/bin/ck" <<'STUB' || exit 1
#!/usr/bin/env bash
case "$*" in
  'module status aft') exit 0 ;;
  *) exit 64 ;;
esac
STUB
chmod +x "$TEST_ROOT/bin/ck" || exit 1
export PATH="$TEST_ROOT/bin:$PATH"
failures=0

run_case() {
  local name=$1 expect=$2
  shift 2
  local output
  output=$(bash "$PLACE_SCRIPT" --module aft --marker x --tool-defs none "$@" 2>&1)
  if printf '%s\n' "$output" | grep -qF "$expect"; then
    echo "PASS: $name"
  else
    echo "FAIL: $name: expected '$expect', got: $output"
    failures=$((failures + 1))
  fi
}

run_case malformed-ask-id-is-refused \
  "REFUSED: --allow-debuggable takes the id of the operator's approving ask" \
  --staged "$MISSING" --allow-debuggable yes
run_case first-install-is-refused \
  "REFUSED: --allow-debuggable is only for placing a staged build over a running module" \
  --staged "$MISSING" --install --allow-debuggable ask_2dcaa2a55df97712
run_case rollback-is-refused \
  "REFUSED: --allow-debuggable is only for placing a staged build over a running module" \
  --rollback 20261107T000000Z --allow-debuggable ask_2dcaa2a55df97712
run_case no-restart-is-refused \
  "REFUSED: --allow-debuggable requires the restart" \
  --staged "$MISSING" --no-restart --allow-debuggable ask_2dcaa2a55df97712
run_case valid-request-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --staged "$MISSING" --allow-debuggable ask_2dcaa2a55df97712

if [ "$failures" -ne 0 ]; then
  echo "$failures place-module --allow-debuggable case(s) failed" >&2
  exit 1
fi
finished=1
