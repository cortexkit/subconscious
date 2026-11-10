#!/usr/bin/env bash
set -euo pipefail
finished=

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
PLACE_SCRIPT="$ROOT/scripts/fleet/place-module.sh"
mkdir -p "$ROOT/target/scratch"
TEST_ROOT=$(mktemp -d "$ROOT/target/scratch/place-module-terminal.XXXXXX")
finish() {
  local rc=$?
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "place-module-terminal.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

CASE_NAME=""; CASE_DIR=""; BIN_DIR=""; STAGING=""; STUBS=""; DEST=""; STAGED=""
STAMP=20200101T000000Z
CONTROL=TERMINAL_TEST_CONTROL

fail() { printf 'FAIL: %s: %s\n' "$CASE_NAME" "$*" >&2; exit 1; }
assert() { "$@" || fail "assertion failed: $*"; }
assert_contains() { [[ "$1" == *"$2"* ]] || fail "expected output to contain: $2; got: $1"; }

write_image() {
  local image="$1" marker="$2"
  cat >"$image" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = --manifest ]; then
  printf '%s\n' '{"cli_faces":[{"name":"demo-face","binary":"ck-demo"}]}'
elif [ "\${1:-}" = --version ]; then
  if [ "\$0" = "\${CK_TEST_DEST:-}" ]; then echo warm-exec >>"\$CK_TEST_CASE/events"; fi
  printf 'ck-demo $marker\n'
fi
# $marker
# $CONTROL
EOF
  chmod +x "$image"
}

new_sandbox() {
  CASE_NAME="$1"
  CASE_DIR="$TEST_ROOT/$CASE_NAME"
  BIN_DIR="$CASE_DIR/bin"; STAGING="$CASE_DIR/staging"; STUBS="$CASE_DIR/stubs"
  DEST="$BIN_DIR/ck-demo"; STAGED="$STAGING/ck-demo"
  mkdir -p "$BIN_DIR" "$STAGING" "$STUBS" "$CASE_DIR/data" "$CASE_DIR/tmp"
  mkdir -p "$CASE_DIR/.local/share/cortexkit/run/logs"
  : >"$CASE_DIR/.local/share/cortexkit/run/logs/demo.stderr.log"
  : >"$CASE_DIR/events"
  write_image "$DEST" OLD_IMAGE
  cp "$DEST" "$CASE_DIR/original"
  write_image "$STAGED" NEW_IMAGE
  (cd "$STAGING" && shasum -a 256 ck-demo >ck-demo.sha256)
  write_image "$STAGING/ck-demo.rollback-$STAMP" ROLLBACK_IMAGE
  (cd "$STAGING" && shasum -a 256 "ck-demo.rollback-$STAMP" >"ck-demo.rollback-$STAMP.sha256")

  cat >"$STUBS/ck" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  '--ck-cli-face-reserved-names') echo module; exit 0 ;;
  'module status demo') exit 0 ;;
  '--json provenance demo')
    echo provenance >>"$CK_TEST_CASE/events"
    echo '{"modules":[{"daemon_observed":{"pid":4242}}]}' ;;
  'module stop demo')
    echo stop >>"$CK_TEST_CASE/events"
    if [ "$CK_TEST_MODE" = stop-fails ]; then echo 'stop failed'; exit 1; fi
    touch "$CK_TEST_CASE/stopped" ;;
  'module start demo'|'module restart demo') echo "$2" >>"$CK_TEST_CASE/events" ;;
  *) echo "unexpected ck: $*" >&2; exit 64 ;;
esac
EOF
  cat >"$STUBS/ps" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  '-o lstart= -p 4242')
    if [ ! -f "$CK_TEST_CASE/stopped" ]; then
      echo record-start >>"$CK_TEST_CASE/events"
      echo 'Mon Jan  1 00:00:00 2024'
    else
      echo check-pid >>"$CK_TEST_CASE/events"
      case "$CK_TEST_MODE" in
        pid-remains) echo 'Mon Jan  1 00:00:00 2024' ;;
        reused) echo 'Tue Jan  2 00:00:00 2024' ;;
        *) exit 1 ;;
      esac
    fi ;;
  '-o pgid= -p 4242')
    echo record-group >>"$CK_TEST_CASE/events"
    echo '  4343' ;;
  '-axo pgid=,pid=')
    echo check-group >>"$CK_TEST_CASE/events"
    count=0
    if [ -f "$CK_TEST_CASE/polls" ]; then read -r count <"$CK_TEST_CASE/polls"; fi
    count=$((count + 1))
    echo "$count" >"$CK_TEST_CASE/polls"
    # An unrelated process must not prevent placement. A kept host remains
    # in the recorded group even after the module pid has disappeared.
    echo '9999 8888'
    case "$CK_TEST_MODE" in
      never-empty) echo '4343 4545' ;;
      delayed) if [ "$count" -lt 3 ]; then echo '4343 4545'; fi ;;
      ps-fails) exit 2 ;;
    esac ;;
  *) echo "unexpected ps: $*" >&2; exit 64 ;;
esac
EOF
  cat >"$STUBS/mv" <<'EOF'
#!/usr/bin/env bash
if [ "${2:-}" = "$CK_TEST_DEST" ]; then echo place >>"$CK_TEST_CASE/events"; fi
exec "$CK_TEST_REAL_MV" "$@"
EOF
  cat >"$STUBS/sleep" <<'EOF'
#!/usr/bin/env bash
echo "sleep $*" >>"$CK_TEST_CASE/events"
EOF
  cat >"$STUBS/sysctl" <<'EOF'
#!/usr/bin/env bash
echo tcp-sample >>"$CK_TEST_CASE/events"
echo 100
EOF
  cat >"$STUBS/uname" <<'EOF'
#!/usr/bin/env bash
echo Darwin
EOF
  cat >"$STUBS/file" <<'EOF'
#!/usr/bin/env bash
echo 'ELF 64-bit LSB executable'
EOF
  cat >"$STUBS/strings" <<'EOF'
#!/usr/bin/env bash
cat "$1"
EOF
  cat >"$STUBS/sqlite3" <<'EOF'
#!/usr/bin/env bash
echo ok
EOF
  for tool in nm codesign; do
    printf '#!/usr/bin/env bash\nexit 0\n' >"$STUBS/$tool"
  done
  for tool in pgrep kill; do
    printf '#!/usr/bin/env bash\nexit 64\n' >"$STUBS/$tool"
  done
  chmod +x "$STUBS/"*
}

invoke() {
  local mode="$1" direction="$2"
  shift 2
  local args=(--staged "$STAGED" --marker NEW_IMAGE --control "$CONTROL" --tool-defs none)
  if [ "$direction" = rollback ]; then args=(--rollback "$STAMP"); fi
  CK_BIN_DIR="$BIN_DIR" CK_STAGING="$STAGING" CK_TEST_CASE="$CASE_DIR" \
  CK_TEST_DEST="$DEST" CK_TEST_MODE="$mode" CK_TEST_REAL_MV="$(command -v mv)" \
  HOME="$CASE_DIR" TMPDIR="$CASE_DIR/tmp" XDG_DATA_HOME="$CASE_DIR/data" PATH="$STUBS:$PATH" \
    bash "$PLACE_SCRIPT" --module demo --dest "$DEST" "${args[@]}" --terminal "$@" # nonempty-array: args contains staged flags or rollback stamp
}

assert_events() {
  local actual expected
  actual=$(cat "$CASE_DIR/events")
  expected=$(printf '%s\n' "$@")
  [ "$actual" = "$expected" ] || fail "expected events [$expected], got [$actual]"
}

assert_placed() {
  assert cmp -s "$DEST" "$1"
  assert test -L "$BIN_DIR/ck-demo-face"
  assert test "$(readlink "$BIN_DIR/ck-demo-face")" = ck-demo
}

case_terminal_forward() {
  new_sandbox terminal-forward
  local output
  output=$(invoke delayed forward --place) || fail "placement failed: $output"
  assert_placed "$STAGED"
  assert_contains "$output" 'terminal placement: kept host processes were ended'
  assert_events provenance record-start record-group stop \
    check-pid check-group 'sleep 1' check-pid check-group 'sleep 1' check-pid check-group \
    place warm-exec tcp-sample start 'sleep 20' tcp-sample
}

case_terminal_rollback() {
  new_sandbox terminal-rollback
  local output
  output=$(invoke reused rollback --place) || fail "rollback failed: $output"
  assert_placed "$STAGING/ck-demo.rollback-$STAMP"
  assert_contains "$output" 'terminal placement: kept host processes were ended'
  assert_events provenance record-start record-group stop check-pid check-group place warm-exec start
}

case_terminal_restore_store() {
  new_sandbox terminal-restore-store
  local output
  printf 'old store\n' >"$CASE_DIR/store.db"
  printf 'restored store\n' >"$STAGING/demo.store.db.rollback-$STAMP"
  output=$(invoke empty rollback --restore-store "$CASE_DIR/store.db" --place) || fail "rollback failed: $output"
  assert_placed "$STAGING/ck-demo.rollback-$STAMP"
  assert cmp -s "$CASE_DIR/store.db" "$STAGING/demo.store.db.rollback-$STAMP"
  assert_events provenance record-start record-group stop check-pid check-group place warm-exec start
}

case_never_empties() {
  local mode="$1" direction="$2" output rc=0
  new_sandbox "$mode-$direction"
  output=$(invoke "$mode" "$direction" --place 2>&1) || rc=$?
  assert test "$rc" -eq 2
  assert_contains "$output" 'container not empty after stop; nothing placed'
  assert cmp -s "$DEST" "$CASE_DIR/original"
  assert test -f "$CASE_DIR/stopped"
  assert test ! -e "$BIN_DIR/ck-demo-face"
  assert test "$(cat "$CASE_DIR/polls")" -eq 61
  assert test "$(grep -c '^sleep 1$' "$CASE_DIR/events")" -eq 60
  assert test "$(grep -c '^stop$' "$CASE_DIR/events")" -eq 1
  if grep -Eq '^(place|start|restart)$' "$CASE_DIR/events"; then fail "placed or restarted after timeout"; fi
}

case_no_restart() {
  local direction="$1" output rc=0
  new_sandbox "no-restart-$direction"
  output=$(invoke empty "$direction" --no-restart --place 2>&1) || rc=$?
  assert test "$rc" -eq 2
  assert_contains "$output" '--terminal requires starting the module again'
  assert cmp -s "$DEST" "$CASE_DIR/original"
  assert_events
}

case_dry_run() {
  local direction="$1" output
  new_sandbox "dry-run-$direction"
  output=$(invoke empty "$direction") || fail "dry run failed: $output"
  assert_contains "$output" 'NOTHING placed and NOTHING restarted'
  assert cmp -s "$DEST" "$CASE_DIR/original"
  assert_events
}

case_stop_fails() {
  new_sandbox stop-fails
  local output rc=0
  output=$(invoke stop-fails forward --place 2>&1) || rc=$?
  assert test "$rc" -eq 2
  assert_contains "$output" "could not stop 'demo' before terminal placement"
  assert cmp -s "$DEST" "$CASE_DIR/original"
  assert_events provenance record-start record-group stop
}

run_case() {
  case "$1" in
    terminal-forward) case_terminal_forward ;;
    terminal-rollback) case_terminal_rollback ;;
    terminal-restore-store) case_terminal_restore_store ;;
    container-never-empties) case_never_empties never-empty forward ;;
    rollback-container-never-empties) case_never_empties never-empty rollback ;;
    pid-remains) case_never_empties pid-remains forward ;;
    ps-fails) case_never_empties ps-fails forward ;;
    no-restart-forward) case_no_restart forward ;;
    no-restart-rollback) case_no_restart rollback ;;
    dry-run-forward) case_dry_run forward ;;
    dry-run-rollback) case_dry_run rollback ;;
    stop-fails) case_stop_fails ;;
    *) printf 'unknown test case: %s\n' "$1" >&2; exit 2 ;;
  esac
  printf 'PASS: %s\n' "$1"
}

if [ "$#" -eq 0 ] || [ "$1" = all ]; then
  for test_case in terminal-forward terminal-rollback terminal-restore-store \
    container-never-empties rollback-container-never-empties pid-remains ps-fails \
    no-restart-forward no-restart-rollback dry-run-forward dry-run-rollback stop-fails; do
    run_case "$test_case"
  done
else
  run_case "$1"
fi
finished=1
