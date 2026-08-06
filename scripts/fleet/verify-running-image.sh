#!/usr/bin/env bash
# Is each supervised module executing the file at its deploy path?
#
# The failure this catches: replacing a binary in place leaves the running
# process on the OLD, now-unlinked file while every path-derived instrument --
# file hash, mtime, `ps -o comm=` -- reports the new one. Staging a binary
# without restarting produces exactly that state deliberately, so this check
# says FAIL for every module between staging and its restart. That is correct.
#
# What it does NOT check: whether the file on disk is the build you intended.
# Same inode proves the process is running the deployed file, and says nothing
# about which build that file is. Marker-with-control answers that, and the two
# questions need two checks.
set -uo pipefail

BIN="$HOME/.local/share/cortexkit/bin"
CONFIG="${SUBC_CONFIG:-$HOME/.config/cortexkit/subc.jsonc}"

fail=0
checked=0

for path in "$BIN"/ck-*; do
  [ -f "$path" ] || continue
  case "$path" in *.pre-*|*.bak-*|*.[0-9]*Z*) continue ;; esac   # backups
  name=$(basename "$path")

  # Resolve the pid by EXACT executable path, through ps. Two distinct reasons,
  # both of which make pgrep unusable here:
  #
  # Matching a command-line substring (`pgrep -f`) returns this script's own
  # process and any editor or shell that mentions the name. In a verifier that
  # is a wrong answer; in a reaper it is a self-kill.
  #
  # And pgrep on this platform SILENTLY EXCLUDES ITS OWN ANCESTORS. Run from an
  # agent shell, the ancestry is bash -> ck-aft -> ck-subc, so `pgrep -x ck-aft`
  # and `pgrep -x ck-subc` both return EMPTY while both processes are live --
  # reported as "not running" rather than "cannot tell". The blindness lands on
  # exactly the processes most likely to be hosting whatever runs this, which
  # are also the two whose staleness matters most. Measured on this host: ps
  # finds both, pgrep finds neither.
  pid=$(ps -Ao pid=,comm= | awk -v p="$path" '$2==p{print $1; exit}')

  if [ -z "$pid" ]; then
    printf '  %-22s not running\n' "$name"
    continue
  fi

  # Validate the pid BEFORE lsof. `lsof -p ""` does not error: it ignores the
  # empty argument and lists every process on the machine, so the path filter
  # below finds the binary among them and the check reports a MATCH for a
  # module that is not running. A false all-clear, and the worst direction.
  ps -p "$pid" >/dev/null 2>&1 || { printf '  %-22s stale pid %s\n' "$name" "$pid"; fail=1; continue; }

  # Take the inode lsof REPORTS -- the kernel's record of the open file. Do not
  # re-stat the path lsof prints: the path is the SAME STRING whether or not the
  # binary was replaced underneath the process, so comparing the deploy path
  # against itself passes unconditionally, including in the exact state this
  # check exists to detect.
  #
  # `-d txt` returns several rows (the binary, the dynamic loader, mapped data
  # files), so anchor on the path rather than taking the first or last line.
  # Inode is the second-to-last field; the last is the path.
  running=$(lsof -p "$pid" -a -d txt 2>/dev/null | awk -v p="$path" '$NF==p{print $(NF-1)}' | head -1)
  # -L so a symlinked deploy path yields the target's inode, not the link's.
  ondisk=$(stat -Lf '%i' "$path" 2>/dev/null)

  checked=$((checked + 1))
  if [ -z "$running" ]; then
    printf '  %-22s could not read running inode (pid %s)\n' "$name" "$pid"
    fail=1
  elif [ "$running" = "$ondisk" ]; then
    printf '  %-22s ok\n' "$name"
  else
    printf '  %-22s MISMATCH  running=%s deployed=%s\n' "$name" "$running" "$ondisk"
    fail=1
  fi
done

# Report what was examined, not only what was found. A clean result over a
# silently truncated set is indistinguishable from a clean result over the
# whole one.
printf '\n  %s binary/binaries compared against their running process\n' "$checked"
[ -f "$CONFIG" ] || printf '  note: %s absent, so nothing cross-checked the configured module set\n' "$CONFIG"

exit $fail
