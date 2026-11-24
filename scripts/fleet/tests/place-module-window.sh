#!/usr/bin/env bash
set -euo pipefail
finished=

ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
mkdir -p "$ROOT/target/scratch"
TEST_ROOT=$(mktemp -d "$ROOT/target/scratch/place-module-window.XXXXXX")
finish() {
  local rc=$?
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo 'FAIL: place-module-window.sh exited before finishing' >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT
WINDOW_ID=20260102-030405-ab12
STAMP=20200101T000000Z

fail() { printf 'FAIL: %s: %s\n%s\n' "$name" "$*" "${output:-}" >&2; exit 1; }
check() { "$@" || fail "assertion failed: $*"; }
contains() { [[ "$output" == *"$1"* ]] || fail "expected '$1' in: $output"; }
events() {
  local actual expected=''
  actual=$(<"$case_dir/events")
  if [ "$#" -gt 0 ]; then expected=$(printf '%s\n' "$@"); fi
  [ "$actual" = "$expected" ] || fail "expected events [$expected], got [$actual]"
}
unplaced() { check cmp -s "$dest" "$case_dir/original"; }
scratch_removed() {
  if compgen -G "$stage/.window-placement.*" >/dev/null; then fail 'store staging directory was retained'; fi
}

stubs="$TEST_ROOT/stubs"
mkdir "$stubs"
cat >"$stubs/ck" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
note() { echo "$*" >>"$CASE_DIR/events"; }
case "$*" in
  'window current')
    note current
    if [ "$CURRENT_RC" -ne 0 ]; then exit "$CURRENT_RC"; fi
    echo "$TEST_WINDOW" ;;
  'module status demo') ;;
  '--json config status') printf '{"status":{"state":"%s","development_switches":["dev-placement"]}}\n' "$GATE_STATE" ;;
  '--ck-cli-face-reserved-names') echo module ;;
  '--json provenance demo') echo '{"modules":[{"daemon_observed":{"pid":4242}}]}' ;;
  'module stop demo') note stop; touch "$CASE_DIR/stopped" ;;
  'module restart demo') note restart; echo restarted ;;
  'module start demo') note start; echo started ;;
  window\ record\ *)
    [ "$4 $5 $6 $7 $8 $9" = "--window $TEST_WINDOW --owner-pid $TEST_OWNER --module demo" ]
    kind=$3
    shift 9
    case "$kind $*" in
      "store --store $CASE_DIR/store.db --snapshot "*)
        [ "$#" -eq 4 ]
        [ "$(sqlite3 "$4" 'SELECT value FROM demo')" = original ]
        kind=store ;;
      "file --target $DEST --new $STAGED --role binary")
        cmp -s "$DEST" "$CASE_DIR/original"
        kind=binary ;;
      "file --target $DEST.format-versions.json --new $STAGED.format-versions.json --role format-map")
        [ "$(<"$DEST.format-versions.json")" = '{}' ]
        kind=replace-map ;;
      "file --target $DEST.format-versions.json --remove --role format-map")
        [ -f "$DEST.format-versions.json" ]
        kind=remove-map ;;
      *) echo "unexpected record: $kind $*" >&2; exit 64 ;;
    esac
    note "record $kind"
    if [ "$kind" = "$FAIL_RECORD" ]; then exit 2; fi
    # Exercise a journal that consumes the scratch snapshot on success.
    if [ "$kind" = store ]; then mv "$4" "$CASE_DIR/saved-store.db"; fi
    index=1
    if [ -f "$CASE_DIR/index" ]; then index=$(($(<"$CASE_DIR/index") + 1)); fi
    printf '%s\n' "$index" >"$CASE_DIR/index"
    printf '%s\n' "$kind" >"$CASE_DIR/step-$index"
    echo "$index" ;;
  window\ done\ *)
    [ "$4 $5 $6 $7" = "--window $TEST_WINDOW --owner-pid $TEST_OWNER" ]
    [ "$#" -eq 7 ]
    case "$(<"$CASE_DIR/step-$3")" in
      store) ;;
      binary) cmp -s "$DEST" "$STAGED" ;;
      replace-map) cmp -s "$DEST.format-versions.json" "$STAGED.format-versions.json" ;;
      remove-map) [ ! -e "$DEST.format-versions.json" ] ;;
    esac
    note "done $3"
    if [ "$3" = "$FAIL_DONE" ]; then exit 2; fi ;;
  *) echo "unexpected ck: $*" >&2; exit 64 ;;
esac
EOF
cat >"$stubs/ps" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  '-o lstart= -p 4242') [ ! -f "$CASE_DIR/stopped" ] && echo 'Mon Jan  1 00:00:00 2024' ;;
  '-o pgid= -p 4242') echo 4343 ;;
  '-axo pgid=,pid=') echo '9999 8888' ;;
  *) exit 64 ;;
esac
EOF
printf '#!/bin/bash\necho "ELF executable"\n' >"$stubs/file"
printf '#!/bin/bash\ncat "$1"\n' >"$stubs/strings"
for tool in nm codesign sysctl; do printf '#!/bin/bash\nexit 0\n' >"$stubs/$tool"; done
chmod +x "$stubs/"*

new_case() {
  name=$1
  case_dir="$TEST_ROOT/$name"
  stage="$case_dir/stage"
  dest="$case_dir/bin/ck-demo"
  staged="$stage/ck-demo"
  mkdir -p "$case_dir"/{bin,data,run,config,tmp,stage,.local/share/cortexkit/run/logs}
  : >"$case_dir/events"
  : >"$case_dir/.local/share/cortexkit/run/logs/demo.stderr.log"
  printf '#!/bin/bash\necho old-image\n# common-control\n' >"$dest"
  cp "$dest" "$case_dir/original"
  cat >"$staged" <<'EOF'
#!/bin/bash
case "${1:-}" in
  --manifest) printf '%s\n' "${MANIFEST:-"{}"}" ;;
  --version) echo new-image ;;
esac
# unique-marker common-control
EOF
  chmod +x "$dest" "$staged"
  (cd "$stage" && shasum -a 256 ck-demo >ck-demo.sha256)
}

invoke() {
  local command_path=${COMMAND_PATH:-"$stubs:$PATH"}
  rc=0
  output=$(HOME="$case_dir" TMPDIR="$case_dir/tmp" PATH="$command_path" \
    XDG_DATA_HOME="$case_dir/data" XDG_RUNTIME_DIR="$case_dir/run" XDG_CONFIG_HOME="$case_dir/config" \
    CK_STAGING="$stage" CK_BIN_DIR="$case_dir/bin" \
    CASE_DIR="$case_dir" DEST="$dest" STAGED="$staged" TEST_WINDOW="$WINDOW_ID" \
    CURRENT_RC="${CURRENT_RC:-0}" GATE_STATE="${GATE_STATE:-unstamped}" \
    FAIL_RECORD="${FAIL_RECORD:-}" FAIL_DONE="${FAIL_DONE:-}" MANIFEST="${MANIFEST:-}" \
    /bin/bash -c 'export TEST_OWNER=$$; exec /bin/bash "$@"' _ "$ROOT/scripts/fleet/place-module.sh" \
      --module demo --staged "$staged" --marker unique-marker --control common-control --tool-defs none "$@" 2>&1) || rc=$?
}

pass() { check test "$rc" -eq "$1"; echo "PASS: $name"; }

new_case terminal-migration-replaces-map
sqlite3 "$case_dir/store.db" "CREATE TABLE demo(value); INSERT INTO demo VALUES ('original');"
printf '{}\n' >"$dest.format-versions.json"
printf '{ }\n' >"$staged.format-versions.json"
invoke --place --terminal --migrates "$case_dir/store.db"
events current 'record store' 'done 1' 'record binary' stop 'done 2' 'record replace-map' start 'done 3'
scratch_removed
contains "store snapshot is held by window $WINDOW_ID"
check cmp -s "$dest" "$staged"
check cmp -s "$dest.format-versions.json" "$staged.format-versions.json"
pass 0

new_case removes-map-without-restart
printf '{}\n' >"$dest.format-versions.json"
invoke --place --no-restart
events current 'record binary' 'done 1' 'record remove-map' 'done 2'
check test ! -e "$dest.format-versions.json"
pass 0

new_case no-map-delays-binary-done-until-restart
invoke --place
events current 'record binary' restart 'done 1'
pass 0

new_case first-record-refusal-precedes-terminal-stop
FAIL_RECORD=binary invoke --place --terminal
unplaced
events current 'record binary'
contains "window $WINDOW_ID (steps recorded: none)"
pass 2

new_case later-record-refusal-preserves-completed-binary
printf '{}\n' >"$dest.format-versions.json"
FAIL_RECORD=remove-map invoke --place
events current 'record binary' 'done 1' 'record remove-map'
contains "window $WINDOW_ID (steps recorded: 1)"
check cmp -s "$dest" "$staged"
check test -f "$dest.format-versions.json"
pass 2

new_case done-refusal-names-index-after-restart
FAIL_DONE=1 invoke --place
events current 'record binary' restart 'done 1'
check cmp -s "$dest" "$staged"
contains "window $WINDOW_ID step 1"
pass 1

for current_rc in 1 2; do
  new_case "current-exit-$current_rc-refuses-before-placement"
  CURRENT_RC=$current_rc invoke --place --terminal
  unplaced
  events current
  contains "ck window current failed (exit $current_rc)"
  pass 2
done

new_case missing-ck-refuses-before-placement
COMMAND_PATH="$case_dir/bin" invoke --place --no-restart
unplaced
events
contains 'ck is required'
pass 2

new_case dry-run-never-checks-window
invoke --no-restart
unplaced
events
contains 'NOTHING placed and NOTHING restarted'
pass 0

new_case stamped-install-is-refused-even-with-dev-placement
GATE_STATE=stamped invoke --place --no-restart
unplaced
events current
contains "stamped install is not supported in window $WINDOW_ID"
pass 2

new_case face-creation-is-refused
MANIFEST='{"cli_faces":[{"name":"demo-face","binary":"ck-demo"}]}' invoke --place --no-restart
unplaced
events current
check test ! -L "$case_dir/bin/ck-demo-face"
contains "creating or removing CLI faces is not supported in window $WINDOW_ID"
pass 2

new_case face-removal-is-refused
ln -s ck-demo "$case_dir/bin/ck-demo-face"
MANIFEST='{"cli_faces":[]}' invoke --place --no-restart
unplaced
events current
check test -L "$case_dir/bin/ck-demo-face"
contains "creating or removing CLI faces is not supported in window $WINDOW_ID"
pass 2

new_case install-is-refused
rm "$dest"
invoke --place --install
events current
check test ! -e "$dest"
contains "--install is not supported in window $WINDOW_ID"
pass 2

new_case rollback-is-refused
invoke --place --rollback "$STAMP" --no-restart
unplaced
events current
contains "--rollback is not supported in window $WINDOW_ID"
pass 2

new_case no-window-keeps-legacy-rollback
CURRENT_RC=3 invoke --place --no-restart
events current
check cmp -s "$dest" "$staged"
contains 'matches live'
legacy_copy=$(compgen -G "$stage/ck-demo.rollback-*Z")
check cmp -s "$legacy_copy" "$case_dir/original"
check test -f "$legacy_copy.sha256"
pass 0
finished=1
