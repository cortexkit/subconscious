#!/usr/bin/env bash
set -euo pipefail
finished=

if [ "$(uname -s)" != Darwin ]; then
  echo 'SKIP: cut-daemon.sh is macOS only'
  exit 0
fi

ROOT=$(cd "$(dirname "$0")/../../.." && pwd)
mkdir -p "$ROOT/target/scratch"
TEST_ROOT=$(mktemp -d "$ROOT/target/scratch/cut-daemon-window.XXXXXX")
finish() {
  local rc=$?
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo 'FAIL: cut-daemon-window.sh exited before finishing' >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT
WINDOW_ID=20260102-030405-ab12
REAL_AWK=$(command -v awk)

fail() { printf 'FAIL: %s: %s\n%s\n' "$name" "$*" "${output:-}" >&2; exit 1; }
check() { "$@" || fail "assertion failed: $*"; }
contains() { [[ "$output" == *"$1"* ]] || fail "expected '$1' in: $output"; }
events() {
  local actual expected
  actual=$(<"$case_dir/events")
  expected=$(printf '%s\n' "$@")
  [ "$actual" = "$expected" ] || fail "expected events [$expected], got [$actual]"
}

stubs="$TEST_ROOT/stubs"
mkdir "$stubs"
cat >"$stubs/ck" <<'EOF'
#!/bin/bash
set -euo pipefail
note() { echo "$*" >>"$CASE_DIR/events"; }
case "$*" in
  'stack status --json')
    note status
    [ "$XDG_DATA_HOME" = "$CASE_DIR/data" ]
    if [ "$STATUS_RC" -ne 0 ]; then echo 'fixture status failed' >&2; exit "$STATUS_RC"; fi
    if [ -n "$STATUS_OVERRIDE" ]; then printf '%s\n' "$STATUS_OVERRIDE"; exit 0; fi
    if [ -f "$CASE_DIR/live" ]; then
      source=daemon; manager=running
    else
      source=manager+roster; manager=not_loaded
    fi
    printf '{"schema":"ck.stack.status/v1","source":"%s","manager":"%s","stopped_marker":"%s"}\n' "$source" "$manager" "$MARKER" ;;
  'window current')
    note current
    if [ "$CURRENT_RC" -ne 0 ]; then exit "$CURRENT_RC"; fi
    echo "$TEST_WINDOW" ;;
  "window record activation --target daemon --window $TEST_WINDOW --owner-pid $TEST_OWNER")
    note record
    if [ "$RECORD_RC" -ne 0 ]; then exit "$RECORD_RC"; fi
    echo 7 ;;
  'daemon announce-restart') note announce ;;
  "window done 7 --window $TEST_WINDOW --owner-pid $TEST_OWNER")
    note done
    exit "$DONE_RC" ;;
  *) echo "unexpected ck: $*" >&2; exit 64 ;;
esac
EOF
cat >"$stubs/launchctl" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\n' "$1" >>"$CASE_DIR/events"
case "$1" in
  print)
    [ "$2" = "gui/$(id -u)/cortexkit.subc" ]
    [ -f "$CASE_DIR/live" ]
    printf '\tpid = %s\n' "$(<"$CASE_DIR/live")" ;;
  bootout)
    [ "$2" = "gui/$(id -u)/cortexkit.subc" ]
    if [ ! -f "$CASE_DIR/live" ]; then echo 'not loaded'; exit 1; fi
    rm "$CASE_DIR/live" ;;
  bootstrap)
    [ "$2" = "gui/$(id -u)" ]
    [ "$3" = "$HOME/Library/LaunchAgents/cortexkit.subc.plist" ]
    printf '%s\n' "$NEW_PID" >"$CASE_DIR/live"
    echo '2026-01-02T03:04:05.000Z subc daemon starting' >"$LOG_FILE" ;;
  *) exit 64 ;;
esac
EOF
cat >"$stubs/date" <<'EOF'
#!/bin/bash
case "$*" in
  '-u +%H:%M:%SZ') echo 03:04:05Z ;;
  '-u +%Y-%m-%dT%H:%M:%S') echo 2026-01-02T03:04:05 ;;
  '-u +%F') echo 2026-01-02 ;;
  *) exit 64 ;;
esac
EOF
cat >"$stubs/awk" <<'EOF'
#!/bin/bash
if [ "$1" = '-v' ]; then echo check-log >>"$CASE_DIR/events"; fi
exec "$REAL_AWK" "$@"
EOF
cat >"$stubs/old-pid-probe" <<'EOF'
kill() {
  [ "$*" = '-0 999999999' ] || return 64
  echo probe-old-pid >>"$CASE_DIR/events"
  [ ! -f "$CASE_DIR/old-exited" ]
}
EOF
cat >"$stubs/sleep" <<'EOF'
#!/bin/bash
if [ "$WAIT_FOR_OLD" -eq 1 ]; then
  echo "sleep $*" >>"$CASE_DIR/events"
  touch "$CASE_DIR/old-exited"
fi
EOF
printf '#!/bin/bash\necho systemctl >>"$CASE_DIR/events"\nexit 64\n' >"$stubs/systemctl"
chmod +x "$stubs/"*

new_case() {
  name=$1
  case_dir="$TEST_ROOT/$name"
  log_dir="$case_dir/.local/share/cortexkit/run/logs"
  mkdir -p "$case_dir"/{data,run,config,Library/LaunchAgents} "$log_dir"
  : >"$case_dir/Library/LaunchAgents/cortexkit.subc.plist"
  : >"$case_dir/events"
  echo 999999999 >"$case_dir/live"
}

invoke() {
  local bash_env=''
  if [ "${WAIT_FOR_OLD:-0}" -eq 1 ]; then bash_env="$stubs/old-pid-probe"; fi
  rc=0
  output=$(HOME="$case_dir" PATH="$stubs:$PATH" BASH_ENV="$bash_env" WAIT_FOR_OLD="${WAIT_FOR_OLD:-0}" \
    XDG_DATA_HOME="$case_dir/data" XDG_RUNTIME_DIR="$case_dir/run" XDG_CONFIG_HOME="$case_dir/config" \
    CASE_DIR="$case_dir" TEST_WINDOW="$WINDOW_ID" REAL_AWK="$REAL_AWK" LOG_FILE="$log_dir/subc.2026-01-02.log" \
    MARKER="${MARKER:-absent}" STATUS_RC="${STATUS_RC:-0}" STATUS_OVERRIDE="${STATUS_OVERRIDE:-}" \
    CURRENT_RC="${CURRENT_RC:-0}" RECORD_RC="${RECORD_RC:-0}" DONE_RC="${DONE_RC:-0}" NEW_PID="${NEW_PID:-999999998}" \
    /bin/bash -c 'export TEST_OWNER=$$; exec /bin/bash "$@"' _ "$ROOT/scripts/fleet/cut-daemon.sh" "$@" 2>&1) || rc=$?
}

pass() { check test "$rc" -eq "$1"; echo "PASS: $name"; }

case_present() {
  local current_rc="$1"
  new_case "present-marker-refuses-current-$current_rc"
  MARKER=present CURRENT_RC=$current_rc invoke
  check test "$(<"$case_dir/live")" = 999999999
  contains 'stopped_marker=present'
  events status
  pass 2
}

case_wait_for_old_pid() {
  new_case window-waits-for-old-pid-after-unload
  WAIT_FOR_OLD=1 invoke
  events status current record print announce bootout probe-old-pid 'sleep 0.5' probe-old-pid \
    print print print bootstrap print check-log 'done'
  pass 0
}

# Run an individual regression case when requested.
case "${1:-}" in
  present-marker) case_present 3; finished=1; exit 0 ;;
  wait-for-old-pid) case_wait_for_old_pid; finished=1; exit 0 ;;
esac

for current_rc in 0 3; do
  case_present "$current_rc"

  new_case "unknown-marker-refuses-current-$current_rc"
  MARKER=unknown CURRENT_RC=$current_rc invoke
  contains 'stopped_marker=unknown'
  events status
  pass 2

  new_case "status-failure-refuses-current-$current_rc"
  STATUS_RC=1 CURRENT_RC=$current_rc invoke
  contains 'fixture status failed'
  contains 'ck stack status --json failed (exit 1)'
  events status
  pass 2

  new_case "unparseable-status-refuses-current-$current_rc"
  STATUS_OVERRIDE='not JSON' CURRENT_RC=$current_rc invoke
  contains 'did not return a parseable ck.stack.status/v1 report'
  events status
  pass 2
done

case_index=0
for status_override in \
  '{"schema":"ck.stack.status/v0","stopped_marker":"absent"}' \
  '{"schema":"ck.stack.status/v1"}' \
  '{"schema":"ck.stack.status/v1","stopped_marker":"new-state"}' \
  '{"schema":"ck.stack.status/v1","stopped_marker":"present","stopped_marker":"absent"}' \
  '{"schema":"ck.stack.status/v1","stopped_marker":"absent","daemon_pid":NaN}'; do
  case_index=$((case_index + 1))
  new_case "invalid-status-shape-$case_index"
  STATUS_OVERRIDE=$status_override invoke
  contains 'did not return a parseable ck.stack.status/v1 report'
  events status
  pass 2
done

new_case offline-without-marker-can-cut
rm "$case_dir/live"
CURRENT_RC=3 invoke
events status current print announce bootout print print print bootstrap print check-log
contains 'outgoing daemon pid: none'
contains 'NEW daemon pid 999999998'
pass 0

new_case stale-marker-keeps-legacy-output
MARKER=stale CURRENT_RC=3 invoke
events status current print announce bootout print print print bootstrap print check-log
expected='03:04:05Z outgoing daemon pid: 999999999
03:04:05Z outgoing daemon gone (pid alive: 0, job loaded: no)
03:04:05Z bootstrap ok (attempt 1)
03:04:05Z NEW daemon pid 999999998: 2026-01-02T03:04:05.000Z
03:04:05Z verify: inode proc == disk, and '\''ck module list'\'' shows every module running'
check test "$output" = "$expected"
pass 0

new_case window-completes-after-wait-even-with-reused-pid
NEW_PID=999999999 invoke
events status current record print announce bootout print print print bootstrap print check-log 'done'
contains 'NEW daemon pid 999999999'
pass 0

case_wait_for_old_pid

new_case done-refusal-is-a-failed-cut
DONE_RC=2 invoke
events status current record print announce bootout print print print bootstrap print check-log 'done'
contains "could not mark window $WINDOW_ID step 7 done"
if [[ "$output" == *'NEW daemon pid'* ]]; then fail 'reported success after refused done'; fi
pass 1

new_case first-record-refusal-precedes-announce
RECORD_RC=2 invoke
events status current record
contains "could not record daemon activation in window $WINDOW_ID"
pass 2

new_case window-current-failure-precedes-announce
CURRENT_RC=2 invoke
events status current
contains 'ck window current failed (exit 2)'
pass 2

new_case dry-run-does-not-call-ck-or-mutate-launchd
MARKER=present invoke --dry-run
events print
check test "$(<"$case_dir/live")" = 999999999
contains 'dry run: would cut'
pass 0
finished=1
