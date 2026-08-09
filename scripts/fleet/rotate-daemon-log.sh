#!/usr/bin/env bash
# Rotate the shared subc daemon log without restarting the daemon.
#
# WHY THIS EXISTS RATHER THAN A LOGROTATE ENTRY OR A CODE CHANGE. launchd owns
# the file: StandardOutPath and StandardErrorPath in cortexkit.subc.plist both
# point at it, so the daemon never opens it and cannot rotate it. launchd holds
# ONE fd in append mode for the process lifetime. That rules out the usual
# rename-and-recreate rotation -- renaming leaves launchd writing to the renamed
# inode, so the "new" log stays empty forever while the archive grows, which is
# WORSE THAN NOT ROTATING because every reader is then looking at a file that is
# permanently silent and looks healthy.
#
# So this copy-truncates: archive the bytes, then truncate IN PLACE, keeping the
# inode launchd is holding.
#
# THE PROPERTY THAT MAKES TRUNCATION SAFE HERE WAS MEASURED, NOT ASSUMED, because
# it is load-bearing and wrong in the other direction would corrupt the file every
# cycle. With an O_APPEND fd held open across a truncate, the next write lands at
# offset 0. Verified directly, with the negative control that gives the check its
# meaning: the SAME sequence on a non-append fd leaves a sparse hole (13 bytes for
# two 6/7-byte writes) because that fd keeps its own offset. Append and non-append
# differ, so the reading was real rather than a test that passes either way.
# Re-verify with `bash rotate-daemon-log.sh --self-test` if that assumption is ever
# in doubt; it costs nothing and it is the only reason this is not a gamble.
#
# THE RACE IS ACCEPTED AND BOUNDED: lines written between the copy finishing and
# the truncate landing are lost. At ~40 lines/sec that is a handful of lines, and
# the alternative (stopping the daemon) costs far more than it saves. This is a
# LOG, and losing a few lines of it is not the same class of loss as losing data.
#
# ONE ARCHIVE IS KEPT, NOT A SERIES. A rotation series on a file this size fills
# the disk quietly, which is the failure this is meant to prevent rather than
# cause. If the previous archive matters, move it before running.

set -uo pipefail

LOG="${SUBC_DAEMON_LOG:-$HOME/.local/share/cortexkit/run/subc.log}"
ARCHIVE="$LOG.1"
# Sized so ordinary volume never trips it. Measured baseline: a healthy day is
# tens of MB; the flood that motivated this reached 936 MB in ~44h.
THRESHOLD_MB="${ROTATE_THRESHOLD_MB:-200}"

self_test() {
  # Prove the assumption this script rests on, in both directions. A test that
  # only demonstrates the append case would pass whether or not append is what
  # makes it work.
  local t="/tmp/rot-selftest-$$" pass=0
  : > "$t"; exec 9>>"$t"; echo "first" >&9; : > "$t"; echo "second" >&9
  local append_size; append_size=$(stat -f '%z' "$t" 2>/dev/null || stat -c '%s' "$t")
  exec 9>&-

  : > "$t"; exec 8>"$t"; echo "first" >&8; : > "$t"; echo "second" >&8
  local plain_size; plain_size=$(stat -f '%z' "$t" 2>/dev/null || stat -c '%s' "$t")
  exec 8>&-; rm -f "$t"

  printf 'append fd after truncate: %s bytes (expect 7 -- writes reset to offset 0)\n' "$append_size"
  printf 'plain  fd after truncate: %s bytes (expect 13 -- offset kept, sparse hole)\n' "$plain_size"
  if [ "$append_size" -lt "$plain_size" ]; then
    echo "PASS: the two cases are distinguishable and append resets as required"
    pass=0
  else
    echo "FAIL: no difference between append and non-append -- this test proves nothing,"
    echo "      and copy-truncate rotation must NOT be trusted until it is understood"
    pass=1
  fi
  return $pass
}

[ "${1:-}" = "--self-test" ] && { self_test; exit $?; }

if [ ! -f "$LOG" ]; then
  echo "rotate-daemon-log: $LOG absent -- nothing to do (not an error)"
  exit 0
fi

bytes=$(stat -f '%z' "$LOG" 2>/dev/null || stat -c '%s' "$LOG" 2>/dev/null || echo 0)
mb=$(( bytes / 1048576 ))

if [ "$mb" -lt "$THRESHOLD_MB" ]; then
  printf 'rotate-daemon-log: %s MB, under the %s MB threshold -- left alone\n' "$mb" "$THRESHOLD_MB"
  exit 0
fi

# Copy first. If this fails, the log is untouched -- the destructive step must
# never run on the strength of a copy nobody checked.
if ! cp "$LOG" "$ARCHIVE"; then
  echo "rotate-daemon-log: archive copy FAILED -- log left intact, nothing truncated" >&2
  exit 1
fi

archived=$(stat -f '%z' "$ARCHIVE" 2>/dev/null || stat -c '%s' "$ARCHIVE")
if [ "$archived" -lt "$bytes" ]; then
  # A short archive means the copy did not complete. Truncating now would
  # discard exactly the bytes that failed to copy.
  echo "rotate-daemon-log: archive is $archived bytes vs $bytes expected -- NOT truncating" >&2
  exit 1
fi

: > "$LOG"
after=$(stat -f '%z' "$LOG" 2>/dev/null || stat -c '%s' "$LOG")
printf 'rotate-daemon-log: %s MB archived to %s, live log truncated (now %s bytes)\n' \
  "$mb" "$(basename "$ARCHIVE")" "$after"
