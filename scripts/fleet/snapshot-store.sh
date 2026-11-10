#!/usr/bin/env bash
# Snapshot a module's SQLite store before a placement that may migrate it.
#
#   snapshot-store.sh <store.db> <snapshot path>
#
# Writes a consistent, owner-only (0600) copy of <store.db> to <snapshot path>,
# which must not exist yet, and checks it with PRAGMA quick_check. Exits 0 on
# success and 2 with a reason on stderr otherwise; on failure no snapshot is
# left behind.
#
# WHY VACUUM INTO, NOT .backup. The sqlite3 shell's .backup copies the store
# 100 pages per step, and the online backup API restarts from the first page
# whenever another connection writes to the source between steps. A running
# module writes all the time, so on a large store the copy never finishes: on
# 2026-11-09 a 10 GB aft.db snapshot ran 37 minutes at full CPU without ending
# while AFT kept writing. VACUUM INTO reads the whole store inside one read
# transaction, so concurrent writers can't restart it, and in WAL mode they
# aren't blocked by it either. Its copy is compacted (a different page layout
# holding the same rows and schema), which is all a rollback needs.
#
# THE READER IS CHOSEN BY THE ARTIFACT: whether a recovery sidecar exists, not
# whether the module happens to be running.
#   sidecar present -> mode=ro. A read-only open replays the -wal; if it can't,
#                      refuse. Never fall back to immutable=1, which would
#                      succeed and silently omit every transaction in the -wal.
#   no sidecar      -> the store was closed cleanly and there is nothing to
#                      replay, so immutable=1 is the honest reader (mode=ro
#                      refuses here with error 14 on some SQLite builds).
set -u

STORE=${1:?usage: snapshot-store.sh <store.db> <snapshot path>}
OUT=${2:?usage: snapshot-store.sh <store.db> <snapshot path>}

refuse() {
  printf 'REFUSED: %s\n' "$*" >&2
  rm -f "$OUT"
  exit 2
}

[ -f "$STORE" ] || { printf 'REFUSED: %s is not a file\n' "$STORE" >&2; exit 2; }
# VACUUM INTO refuses an existing output, even an empty one. Refuse here first
# so a stale file is never silently treated as this run's snapshot.
[ ! -e "$OUT" ] || { printf 'REFUSED: %s already exists\n' "$OUT" >&2; exit 2; }

if [ -f "$STORE-wal" ] || [ -f "$STORE-journal" ]; then
  uri="file:$STORE?mode=ro"
  reader="mode=ro (a recovery sidecar is present; no immutable fallback)"
else
  uri="file:$STORE?immutable=1"
  reader="immutable=1 (no recovery sidecar: the store was closed cleanly)"
fi

# The path is a SQL string literal inside VACUUM INTO, so double any single
# quote in it.
quoted=${OUT//\'/\'\'}
# A snapshot holds the module's whole database (vendor payloads, transcripts,
# audit rows). umask 077 makes sqlite3 create it owner-only from the start,
# so it is never readable by other accounts, even briefly.
if ! err=$( (umask 077; sqlite3 "$uri" "VACUUM INTO '$quoted'") 2>&1 ); then
  refuse "snapshot of $STORE failed with reader $reader: $err"
fi

[ -s "$OUT" ] || refuse "snapshot $OUT is empty"
# GNU stat first: on Linux `stat -f` succeeds but prints filesystem
# information, so a BSD-first fallback would never run there.
mode=$(stat -c %a "$OUT" 2>/dev/null || stat -f %Lp "$OUT")
[ "$mode" = 600 ] || refuse "snapshot $OUT has mode $mode, not 600"
check=$(sqlite3 "file:$OUT?immutable=1" 'PRAGMA quick_check;' 2>&1)
[ "$check" = ok ] || refuse "snapshot $OUT fails quick_check: $check"
printf 'snapshot %s via %s\n' "$OUT" "$reader"
