#!/usr/bin/env bash
# Reap language servers whose project root has been deleted.
#
# Mason worktrees get reclaimed by alfonso's janitor while a language server is
# still indexing them. The server is never told, so it holds its whole index in
# RAM forever. Two sweeps on 2026-07-25 each recovered ~40 GB.
#
# The classification runs a positive control first: if the cwd probe cannot
# return a live path for a process we know is alive, the probe is broken and an
# empty result would read as "orphan" when it is really "no answer". Refusing to
# act on an unproven instrument is the whole point -- a broken probe here kills
# healthy servers.
#
# Usage: reap-orphan-lsp.sh [--apply]   (default is dry-run)

set -uo pipefail
APPLY=0
[ "${1:-}" = "--apply" ] && APPLY=1


# bash 3.2 (macOS default) has no mapfile; keep this portable.
# Match on the EXECUTABLE, never on a cmdline substring. `pgrep -f rust-analyzer`
# also matches this script, any editor holding the name in argv, and any shell
# whose command line mentions it -- SIGTERM to that set kills bystanders. A live
# instance of exactly that mistake: `pgrep -f ck-aft` returned the probe's own
# pid instead of the module's.
PIDS=()
# NOTE: no `IFS=` here. Setting it empty disables field splitting, so the whole
# ps line lands in $pid and $comm is always empty -- which selects nothing and
# reports a clean fleet. Default IFS is what splits pid from comm.
while read -r pid comm; do
  [ -z "$pid" ] && continue
  [ "$pid" = "$$" ] && continue
  base=${comm##*/}
  case "$base" in
    rust-analyzer|typescript-language-server|tsserver|gopls|pyright|sourcekit-lsp) PIDS+=("$pid") ;;
  esac
done < <(ps -Ao pid=,comm=)
[ ${#PIDS[@]} -eq 0 ] && { echo "no language servers running"; exit 0; }

cwd_of() { lsof -a -p "$1" -d cwd -Fn 2>/dev/null | grep '^n' | sed 's/^n//' | head -1; }

# POSITIVE CONTROL: the probe must produce a live path for at least one process.
# Without this, a probe that returns empty for everything classifies the whole
# fleet as orphaned.
control_ok=0
for p in "${PIDS[@]:0:10}"; do
  c=$(cwd_of "$p")
  if [ -n "$c" ] && [ -d "$c" ]; then control_ok=1; break; fi
done
if [ "$control_ok" -eq 0 ]; then
  echo "REFUSING: cwd probe never returned a live path across 10 processes."
  echo "The instrument cannot produce the 'alive' answer, so every 'orphan' here is a null, not a finding."
  exit 2
fi

alive=0; orphan=0; unreachable=0; orphans=()
for p in "${PIDS[@]}"; do
  c=$(cwd_of "$p")
  # An EMPTY cwd is a null result, not evidence of a dead root. Only a path that
  # is present and gone from disk counts as an orphan.
  if [ -z "$c" ]; then alive=$((alive+1)); continue; fi
  if [ -d "$c" ]; then alive=$((alive+1)); continue; fi
  # A selective reclaim deletes the worktree and leaves its parent. If the parent
  # AND grandparent are also gone, that is a whole tree or mount being
  # unavailable -- a different event, and killing on it would reap healthy
  # servers whose root is merely unreachable right now.
  parent=$(dirname "$c"); grand=$(dirname "$parent")
  if [ ! -d "$parent" ] && [ ! -d "$grand" ]; then
    unreachable=$((unreachable+1)); alive=$((alive+1)); continue
  fi
  orphan=$((orphan+1)); orphans+=("$p")
done

echo "alive roots:  $alive"
echo "orphan roots: $orphan"
[ "$unreachable" -gt 0 ] && echo "unreachable trees (spared, not orphans): $unreachable"
[ "$orphan" -eq 0 ] && exit 0

printf '  %s\n' "${orphans[@]}" | head -30
if [ "$APPLY" -eq 0 ]; then echo "(dry run; pass --apply to reap)"; exit 0; fi

free_before=$(vm_stat | awk '/Pages free/{printf "%.2f", $3*16384/1073741824}')
kill -TERM "${orphans[@]}" 2>/dev/null
sleep 8
survivors=0
for p in "${orphans[@]}"; do ps -p "$p" >/dev/null 2>&1 && survivors=$((survivors+1)); done
sleep 30
free_after=$(vm_stat | awk '/Pages free/{printf "%.2f", $3*16384/1073741824}')
echo "reaped $orphan, survivors after 8s: $survivors"
echo "free GB: $free_before -> $free_after"
