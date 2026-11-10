#!/usr/bin/env bash
# Fixtures for scripts/fleet/snapshot-store.sh, run by `scripts/gate.sh fast`.
# They use the real sqlite3, because the property under test is SQLite's own
# behaviour: a store kept busy by another connection's writes must still
# snapshot in bounded time.
set -euo pipefail
finished=

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
SNAP="$ROOT/scripts/fleet/snapshot-store.sh"
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/snapshot-store.XXXXXX")
writer=
finish() {
  local rc=$?
  [ -z "$writer" ] || kill "$writer" 2>/dev/null || true
  [ -z "$writer" ] || wait "$writer" 2>/dev/null || true
  rm -rf "$TEST_ROOT"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "snapshot-store.sh tests: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
# GNU stat first: on Linux `stat -f` succeeds but describes the filesystem.
mode_of() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }

# run_bounded <seconds> <output file> <command...>: run the command, killing it
# after <seconds>. Returns its status, or 124 if it was killed. Written out
# because macOS runners don't ship `timeout`.
run_bounded() {
  local limit=$1 out=$2 pid waited=0
  shift 2
  "$@" >"$out" 2>&1 &
  pid=$!
  while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge "$((limit * 10))" ]; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      return 124
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  wait "$pid"
}

# A WAL store of about 100 MB, so a page-stepped copy spans many steps.
DB="$TEST_ROOT/store.db"
sqlite3 "$DB" "PRAGMA journal_mode=WAL;
  CREATE TABLE t(id INTEGER PRIMARY KEY, b BLOB);
  WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x<100000)
  INSERT INTO t(b) SELECT randomblob(1000) FROM c;" >/dev/null
rows_before=$(sqlite3 "$DB" 'SELECT count(*) FROM t;')

# 1. A busy store snapshots in bounded time. Another connection commits every
#    10 ms throughout, which keeps restarting sqlite3's .backup; the snapshot
#    must still finish well inside the bound.
python3 - "$DB" <<'PY' &
import sqlite3, sys, time
c = sqlite3.connect(sys.argv[1], isolation_level=None)
end = time.time() + 120
while time.time() < end:
    c.execute("INSERT INTO t(b) VALUES (randomblob(1000))")
    time.sleep(0.01)
PY
writer=$!
for _ in $(seq 1 50); do [ -f "$DB-wal" ] && break; sleep 0.1; done
[ -f "$DB-wal" ] || fail "the writer never opened the store"
OUT="$TEST_ROOT/busy.snapshot"
start=$(date +%s)
rc=0
run_bounded 60 "$TEST_ROOT/busy.log" bash "$SNAP" "$DB" "$OUT" || rc=$?
[ "$rc" -ne 124 ] || fail "busy store: snapshot did not finish within 60 s"
[ "$rc" -eq 0 ] || fail "busy store: snapshot failed (rc=$rc): $(cat "$TEST_ROOT/busy.log")"
out=$(cat "$TEST_ROOT/busy.log")
elapsed=$(( $(date +%s) - start ))
[ "$elapsed" -le 30 ] || fail "busy store: snapshot took ${elapsed}s"
[[ "$out" == *"mode=ro"* ]] || fail "busy store: expected the mode=ro reader, got: $out"
[ "$(mode_of "$OUT")" = 600 ] || fail "busy store: snapshot mode is $(mode_of "$OUT"), not 600"
[ "$(sqlite3 "file:$OUT?immutable=1" 'PRAGMA quick_check;')" = ok ] || fail "busy store: snapshot fails quick_check"
rows=$(sqlite3 "file:$OUT?immutable=1" 'SELECT count(*) FROM t;')
[ "$rows" -ge "$rows_before" ] || fail "busy store: snapshot holds $rows rows, fewer than the $rows_before before the writer started"
kill "$writer" 2>/dev/null || true
wait "$writer" 2>/dev/null || true
writer=

# 2. An existing output is refused and left alone.
before=$(shasum -a 256 "$OUT")
if bash "$SNAP" "$DB" "$OUT" 2>/dev/null; then fail "existing output: expected a refusal"; fi
[ "$(shasum -a 256 "$OUT")" = "$before" ] || fail "existing output: the refused run changed the file"

# 3. A cleanly closed store (no -wal) is read with immutable=1.
CLOSED="$TEST_ROOT/closed.db"
sqlite3 "$CLOSED" "CREATE TABLE t(x); INSERT INTO t VALUES (1),(2),(3);"
[ ! -f "$CLOSED-wal" ] || fail "closed store: fixture unexpectedly has a -wal"
out=$(bash "$SNAP" "$CLOSED" "$TEST_ROOT/closed.snapshot") || fail "closed store: snapshot failed"
[[ "$out" == *"immutable=1"* ]] || fail "closed store: expected the immutable=1 reader, got: $out"
[ "$(sqlite3 "file:$TEST_ROOT/closed.snapshot?immutable=1" 'SELECT count(*) FROM t;')" = 3 ] || fail "closed store: wrong row count"

# 4. A file that isn't a database is refused, and no snapshot is left behind.
printf 'not a database\n' >"$TEST_ROOT/garbage.db"
if bash "$SNAP" "$TEST_ROOT/garbage.db" "$TEST_ROOT/garbage.snapshot" 2>/dev/null; then fail "garbage store: expected a refusal"; fi
[ ! -e "$TEST_ROOT/garbage.snapshot" ] || fail "garbage store: a refused snapshot was left behind"

finished=1
echo "snapshot-store.sh: 4 cases passed"
