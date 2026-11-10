#!/usr/bin/env bash
set -euo pipefail
finished=

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
PLACE_SCRIPT="$ROOT/scripts/fleet/place-module.sh"
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/place-module-rollback.XXXXXX")
finish() {
  local rc=$?
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "place-module-rollback.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

CASE_NAME=""
CASE_DIR=""
BIN_DIR=""
STAGING=""
STUBS=""
DEST=""
STAMP="20200101T000000Z"
CONTROL="ROLLBACK_TEST_CONTROL"

fail() {
  printf 'FAIL: %s: %s\n' "$CASE_NAME" "$*" >&2
  exit 1
}

assert() {
  "$@" || fail "assertion failed: $*"
}

assert_contains() {
  [[ "$1" == *"$2"* ]] || fail "expected output to contain: $2"
}

new_sandbox() {
  CASE_NAME="$1"
  CASE_DIR="$TEST_ROOT/$CASE_NAME"
  BIN_DIR="$CASE_DIR/bin"
  STAGING="$CASE_DIR/staging"
  STUBS="$CASE_DIR/stubs"
  DEST="$BIN_DIR/ck-demo"
  mkdir -p "$BIN_DIR" "$STAGING" "$STUBS" "$CASE_DIR/data"

  cat >"$STUBS/file" <<'EOF'
#!/usr/bin/env bash
printf 'ELF 64-bit LSB executable\n'
EOF
  cat >"$STUBS/nm" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$STUBS/codesign" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$STUBS/strings" <<'EOF'
#!/usr/bin/env bash
cat "$1"
EOF
  cat >"$STUBS/ck" <<'EOF'
#!/usr/bin/env bash
if [ "${1:-}" = "--ck-cli-face-reserved-names" ]; then
  printf '%s\n' activity catalog config daemon health help machine module provenance quota routes setup upgrade
  exit 0
fi
if [ "${1:-}" = module ] && [ "${2:-}" = status ]; then exit 0; fi
if [ "$*" = '--json config status' ]; then
  case "${CK_CONFIG_STATE:-unstamped}" in
    older) printf 'unknown_control_op — config.status\n' >&2; exit 1 ;;
    inactive) printf 'config_gate_inactive — no config store configured\n' >&2; exit 1 ;;
    broken) printf 'daemon unreachable\n' >&2; exit 3 ;;
    malformed) printf '{broken'; exit 0 ;;
    *) printf '{"status":{"state":"%s","development_switches":%s}}\n' "${CK_CONFIG_STATE:-unstamped}" "${CK_CONFIG_SWITCHES:-[]}" ;;
  esac
  exit 0
fi
if [ "${1:-}" = config ] && [ "${2:-}" = place-program ]; then
  printf 'place-program %s\n' "${3:-}" >>"$CK_CALLS"
  [ "${CK_CONFIG_PLACE_REFUSE:-0}" = 0 ] || exit 1
  [ "$(shasum -a 256 "$4" | cut -d' ' -f1)" = "$5" ] || exit 1
  exit 0
fi
if [ "${1:-}" = module ] && { [ "${2:-}" = stop ] || [ "${2:-}" = start ] || [ "${2:-}" = restart ]; }; then
  printf '%s %s\n' "$2" "${3:-}" >>"$CK_CALLS"
  printf 'module %s %s\n' "$2" "${3:-}"
  exit 0
fi
exit 64
EOF
  cat >"$STUBS/sqlite3" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$1 $2" >>"$CK_SQL_CALLS"
if grep -q 'GARBAGE_DATABASE' "$1"; then
  printf 'Error: file is not a database\n' >&2
  exit 1
fi
printf 'ok\n'
EOF
  chmod +x "$STUBS/file" "$STUBS/nm" "$STUBS/codesign" "$STUBS/strings" "$STUBS/ck" "$STUBS/sqlite3"
  : >"$CASE_DIR/ck-calls"
  : >"$CASE_DIR/sqlite-calls"
  write_image "$DEST" '{"cli_faces":[{"name":"agents","binary":"ck-demo"}]}' CURRENT_IMAGE_BYTES
}

write_image() {
  local image="$1" manifest="$2" marker="$3"
  cat >"$image" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = "--manifest" ]; then
  printf '%s\\n' '$manifest'
elif [ "\${1:-}" = "--version" ]; then
  printf 'ck-demo $marker\\n'
fi
# $marker
# $CONTROL
EOF
  chmod +x "$image"
}

make_binary_rollback() {
  write_image "$STAGING/ck-demo.rollback-$STAMP" '{"cli_faces":[{"name":"roles","binary":"ck-demo"}]}' ROLLED_BACK_IMAGE_BYTES
  (cd "$STAGING" && shasum -a 256 "ck-demo.rollback-$STAMP" >"ck-demo.rollback-$STAMP.sha256")
}

invoke() {
  CK_BIN_DIR="$BIN_DIR" \
  CK_STAGING="$STAGING" \
  CK_CALLS="$CASE_DIR/ck-calls" \
  CK_SQL_CALLS="$CASE_DIR/sqlite-calls" \
  XDG_DATA_HOME="$CASE_DIR/data" \
  PATH="$STUBS:$PATH" \
    bash "$PLACE_SCRIPT" --module demo --dest "$DEST" --rollback "$STAMP" "$@"
}

case_rollback_restores_and_saves_current() {
  new_sandbox rollback-restores-and-saves-current
  make_binary_rollback
  local original_digest output candidate found=0
  original_digest=$(shasum -a 256 "$DEST" | awk '{print $1}')
  ln -s ck-demo "$BIN_DIR/ck-agents"
  output=$(invoke --place --no-restart 2>&1) || { printf '%s\n' "$output" >&2; fail "rollback failed"; }
  assert cmp -s "$DEST" "$STAGING/ck-demo.rollback-$STAMP"
  assert_contains "$output" "placed sha"
  assert_contains "$output" "warm-exec at destination: ck-demo ROLLED_BACK_IMAGE_BYTES"
  assert_contains "$output" "rolled back to $STAMP"
  assert_contains "$output" "cli face: removed ck-agents"
  assert_contains "$output" "cli face: created ck-roles"
  assert test ! -e "$BIN_DIR/ck-agents"
  assert test -L "$BIN_DIR/ck-roles"
  assert test "$(readlink "$BIN_DIR/ck-roles")" = ck-demo
  assert test "$(<"$CASE_DIR/ck-calls")" = ""
  for candidate in "$STAGING"/ck-demo.rollback-*; do
    [ -f "$candidate" ] || continue
    [[ "$candidate" == *.sha256 ]] && continue
    [ "$candidate" = "$STAGING/ck-demo.rollback-$STAMP" ] && continue
    if [ "$(shasum -a 256 "$candidate" | awk '{print $1}')" = "$original_digest" ]; then
      assert test -f "$candidate.sha256"
      (cd "$STAGING" && shasum -c "$(basename "$candidate").sha256" >/dev/null) \
        || fail "new current-binary rollback sidecar did not verify"
      found=1
    fi
  done
  assert test "$found" = 1
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_corrupted_sidecar_refused() {
  new_sandbox corrupted-sidecar-refused
  make_binary_rollback
  local before output count=0 candidate
  before=$(shasum -a 256 "$DEST" | awk '{print $1}')
  printf '%064d  ck-demo.rollback-%s\n' 0 "$STAMP" >"$STAGING/ck-demo.rollback-$STAMP.sha256"
  if output=$(invoke --place --no-restart 2>&1); then fail "corrupted rollback unexpectedly succeeded"; fi
  assert_contains "$output" "rollback sidecar does not verify"
  assert test "$(shasum -a 256 "$DEST" | awk '{print $1}')" = "$before"
  for candidate in "$STAGING"/ck-demo.rollback-*; do
    [ -f "$candidate" ] || continue
    [[ "$candidate" == *.sha256 ]] && continue
    count=$((count + 1))
  done
  assert test "$count" = 1
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_missing_stamp_refused() {
  new_sandbox missing-stamp-refused
  local before output
  before=$(shasum -a 256 "$DEST" | awk '{print $1}')
  if output=$(invoke --place --no-restart 2>&1); then fail "missing rollback unexpectedly succeeded"; fi
  assert_contains "$output" "rollback snapshot not found"
  assert test "$(shasum -a 256 "$DEST" | awk '{print $1}')" = "$before"
  assert test "$(ls -A "$STAGING" | wc -l | tr -d ' ')" = 0
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_restart_without_store_uses_daemon() {
  new_sandbox restart-without-store-uses-daemon
  make_binary_rollback
  local output
  output=$(invoke --place 2>&1) || { printf '%s\n' "$output" >&2; fail "rollback restart failed"; }
  assert test "$(<"$CASE_DIR/ck-calls")" = "restart demo"
  assert_contains "$output" "restart initiated -- verify on a lane opened AFTER this point:"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_bad_store_restores_current_files() {
  new_sandbox bad-store-restores-current-files
  make_binary_rollback
  local store="$CASE_DIR/data/demo.store.db" before output
  printf 'CURRENT_DATABASE_BYTES\n' >"$store"
  printf 'CURRENT_WAL_BYTES\n' >"$store-wal"
  printf 'CURRENT_SHM_BYTES\n' >"$store-shm"
  printf 'GARBAGE_DATABASE\n' >"$STAGING/demo.store.db.rollback-$STAMP"
  before=$(shasum -a 256 "$DEST" | awk '{print $1}')
  if output=$(invoke --place --restore-store "$store" 2>&1); then fail "garbage store snapshot unexpectedly succeeded"; fi
  assert_contains "$output" "failed PRAGMA integrity_check"
  assert test "$(<"$store")" = CURRENT_DATABASE_BYTES
  assert test "$(<"$store-wal")" = CURRENT_WAL_BYTES
  assert test "$(<"$store-shm")" = CURRENT_SHM_BYTES
  assert test "$(shasum -a 256 "$DEST" | awk '{print $1}')" = "$before"
  assert test "$(<"$CASE_DIR/ck-calls")" = "stop demo"
  assert test "$(wc -l <"$CASE_DIR/sqlite-calls" | tr -d ' ')" = 1
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_good_store_restarts_module() {
  new_sandbox good-store-restarts-module
  make_binary_rollback
  local store="$CASE_DIR/data/demo.store.db" output saved_dir mode
  printf 'CURRENT_DATABASE_BYTES\n' >"$store"
  printf 'CURRENT_WAL_BYTES\n' >"$store-wal"
  printf 'CURRENT_SHM_BYTES\n' >"$store-shm"
  printf 'VALID_DATABASE_SNAPSHOT\n' >"$STAGING/demo.store.db.rollback-$STAMP"
  output=$(invoke --place --restore-store "$store" 2>&1) \
    || { printf '%s\n' "$output" >&2; fail "valid store rollback failed"; }
  assert test "$(<"$store")" = VALID_DATABASE_SNAPSHOT
  # GNU stat can print filesystem data before rejecting BSD's format option.
  # Separate assignments discard that output when the fallback succeeds.
  mode=$(stat -f %Lp "$store" 2>/dev/null) || mode=$(stat -c %a "$store")
  assert test "$mode" = 600
  assert test "$(<"$CASE_DIR/ck-calls")" = $'stop demo\nstart demo'
  assert_contains "$output" "store integrity_check: ok"
  assert_contains "$output" "restart initiated -- verify on a lane opened AFTER this point:"
  local saved_dirs
  saved_dirs=("$STAGING"/.demo.store-set-aside.*)
  saved_dir="${saved_dirs[0]}"
  assert test -f "$saved_dir/demo.store.db"
  assert test -f "$saved_dir/demo.store.db-wal"
  assert test -f "$saved_dir/demo.store.db-shm"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_dry_run_changes_nothing() {
  new_sandbox dry-run-changes-nothing
  make_binary_rollback
  local before output count=0 candidate
  before=$(shasum -a 256 "$DEST" | awk '{print $1}')
  ln -s ck-demo "$BIN_DIR/ck-agents"
  output=$(invoke --no-restart 2>&1) || { printf '%s\n' "$output" >&2; fail "rollback dry run failed"; }
  assert_contains "$output" "NOTHING placed and NOTHING restarted"
  assert_contains "$output" "would remove ck-agents"
  assert test "$(shasum -a 256 "$DEST" | awk '{print $1}')" = "$before"
  assert test "$(readlink "$BIN_DIR/ck-agents")" = ck-demo
  for candidate in "$STAGING"/ck-demo.rollback-*; do
    [ -f "$candidate" ] || continue
    [[ "$candidate" == *.sha256 ]] && continue
    count=$((count + 1))
  done
  assert test "$count" = 1
  assert test "$(<"$CASE_DIR/ck-calls")" = ""
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_config_gate_preflight() {
  local state before output
  for state in older inactive unstamped stamped broken malformed store_unverified; do
    new_sandbox "config-gate-$state"
    make_binary_rollback
    before=$(shasum -a 256 "$DEST" | awk '{print $1}')
    if output=$(CK_CONFIG_STATE="$state" invoke --place 2>&1); then
      case "$state" in older|inactive|unstamped) ;; *) fail "$state unexpectedly placed" ;; esac
      assert test "$(<"$CASE_DIR/ck-calls")" = 'restart demo'
      assert test "$(shasum -a 256 "$DEST" | awk '{print $1}')" != "$before"
      if [ "$state" = older ] || [ "$state" = inactive ]; then
        assert_contains "$output" 'config gate inactive on this daemon; placing as before'
      fi
    else
      case "$state" in stamped|broken|malformed|store_unverified) ;; *) fail "$state unexpectedly refused: $output" ;; esac
      assert_contains "$output" 'REFUSED:'
      assert test "$(shasum -a 256 "$DEST" | awk '{print $1}')" = "$before"
      assert test "$(<"$CASE_DIR/ck-calls")" = ''
    fi
    printf 'PASS: %s\n' "$CASE_NAME"
  done
}

case_config_gate_placement_order() {
  local refused output
  for refused in 0 1; do
    new_sandbox "config-gate-placement-$refused"
    make_binary_rollback
    if output=$(CK_CONFIG_STATE=stamped CK_CONFIG_SWITCHES='["dev-placement"]' CK_CONFIG_PLACE_REFUSE="$refused" invoke --place 2>&1); then
      [ "$refused" = 0 ] || fail 'refused placement restarted'
      assert test "$(<"$CASE_DIR/ck-calls")" = $'place-program demo\nrestart demo'
    else
      [ "$refused" = 1 ] || fail "placement failed: $output"
      assert test "$(<"$CASE_DIR/ck-calls")" = 'place-program demo'
    fi
    assert_contains "$output" 'equals rollback snapshot'
    printf 'PASS: %s\n' "$CASE_NAME"
  done
}

case_config_gate_forward_and_install() {
  local install output
  for install in 0 1; do
    new_sandbox "config-gate-forward-install-$install"
    write_image "$STAGING/staged" '{"cli_faces":[]}' STAGED_IMAGE_BYTES
    # Execute the production placement tail with the already-passed arm inputs.
    # Signing and currency arms have their own fixtures; no daemon is contacted.
    python3 - "$PLACE_SCRIPT" "$CASE_DIR/tail.sh" <<'PY'
import pathlib,sys
source=pathlib.Path(sys.argv[1]).read_text()
functions=[]
for name in ("config_gate_preflight", "record_program_placement"):
    body=source.split(name+"() {\n",1)[1].split("\n}\n",1)[0]
    functions.append(name+"() {\n"+body+"\n}\n")
tail=source.split('say "=== place"\ncp "$STAGED"',1)[1]
tail='say "=== place"\ncp "$STAGED"'+tail.split('\nif [ "$RESTART" -eq 1 ]; then',1)[0]
pathlib.Path(sys.argv[2]).write_text('set -euo pipefail\nsay() { printf "%s\\n" "$*"; }\nrefuse() { say "REFUSED: $*"; exit 2; }\nreconcile_cli_faces() { :; }\n'+''.join(functions)+'config_gate_preflight\n'+tail)
PY
    if [ "$install" -eq 1 ]; then rm "$DEST"; fi
    output=$(CK_CONFIG_STATE=stamped CK_CONFIG_SWITCHES='["dev-placement"]' \
      CK_CALLS="$CASE_DIR/ck-calls" PATH="$STUBS:$PATH" INSTALL="$install" \
      MODULE=demo DEST="$DEST" STAGED="$STAGING/staged" STAGING="$STAGING" \
      MIGRATES='' PATH_FACE='' bash "$CASE_DIR/tail.sh" 2>&1) \
      || fail "forward placement failed: $output"
    assert_contains "$output" 'equals staged'
    if [ "$install" -eq 1 ]; then
      assert_contains "$output" 'ck config plan'
      assert_contains "$output" 'ck config apply <digest>'
      assert test "$(<"$CASE_DIR/ck-calls")" = ''
      if [[ "$output" == *'ck module rescan'* ]]; then fail 'stamped install named rescan'; fi
    else
      assert test "$(<"$CASE_DIR/ck-calls")" = 'place-program demo'
    fi
    printf 'PASS: %s\n' "$CASE_NAME"
  done
}

run_case() {
  case "$1" in
    config-gate-preflight) case_config_gate_preflight ;;
    config-gate-placement-order) case_config_gate_placement_order ;;
    config-gate-forward-and-install) case_config_gate_forward_and_install ;;
    rollback-restores-and-saves-current) case_rollback_restores_and_saves_current ;;
    corrupted-sidecar-refused) case_corrupted_sidecar_refused ;;
    missing-stamp-refused) case_missing_stamp_refused ;;
    restart-without-store-uses-daemon) case_restart_without_store_uses_daemon ;;
    bad-store-restores-current-files) case_bad_store_restores_current_files ;;
    good-store-restarts-module) case_good_store_restarts_module ;;
    dry-run-changes-nothing) case_dry_run_changes_nothing ;;
    *) printf 'unknown test case: %s\n' "$1" >&2; exit 2 ;;
  esac
}

if [ "$#" -eq 0 ] || [ "$1" = all ]; then
  for test_case in \
    config-gate-preflight \
    config-gate-placement-order \
    config-gate-forward-and-install \
    rollback-restores-and-saves-current \
    corrupted-sidecar-refused \
    missing-stamp-refused \
    restart-without-store-uses-daemon \
    bad-store-restores-current-files \
    good-store-restarts-module \
    dry-run-changes-nothing; do
    run_case "$test_case"
  done
else
  run_case "$1"
fi
finished=1
