#!/usr/bin/env bash
# A module that re-executes its own binary path while it runs must not have
# that binary swapped without a restart, so place-module.sh refuses
# --no-restart for it unless --reexec-safe says its owner confirmed otherwise.
# Other modules are unaffected. Each case stops before touching anything: the
# staged path doesn't exist, so a case that gets past the guard ends on the
# staged-artifact refusal instead.
set -u
ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
PLACE_SCRIPT="$ROOT/scripts/fleet/place-module.sh"
MISSING=$(mktemp -u "${TMPDIR:-/tmp}/place-module-reexec.XXXXXX")
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
  "REFUSED: --no-restart: aft re-executes its own binary path" \
  --module aft --no-restart
run_case reexec-safe-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module aft --no-restart --reexec-safe
run_case reexec-module-with-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module aft
run_case other-module-no-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module basal --no-restart
run_case second-listed-module-refuses-no-restart \
  "REFUSED: --no-restart: cerebellum re-executes its own binary path" \
  --module cerebellum --no-restart
# The harness pins its own executable, so it is no longer listed.
run_case pinned-harness-no-restart-passes-the-guard \
  "REFUSED: staged artifact not found" \
  --module thalamus-harness --no-restart

[ "$failures" -eq 0 ] || exit 1
