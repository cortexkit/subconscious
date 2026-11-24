#!/bin/bash
# Restart the launchd-managed subc daemon onto the binary already placed at its
# path, and do not return until a NEW daemon is running and serving.
#
# Why this exists: the old one-liner was `launchctl bootout …; sleep 2;
# launchctl bootstrap …`. That only worked while the outgoing daemon exited
# within 2 s. Since the daemon waits for each module's own shutdown (up to 25 s
# before it signals a module), bootout can still be waiting when bootstrap runs;
# bootstrap then fails with "Input/output error" because the job is still loaded,
# nothing retries, and the whole fleet stays down until someone notices. That is
# what happened on 2026-09-24 at 07:19Z: 4 minutes dark, ended by the operator.
#
# So this script waits for the old job to be gone, retries bootstrap and checks
# for a fresh daemon start line before it reports success.
# Usage: cut-daemon.sh [--dry-run]  (preview without calling ck or changing launchd)
# Run it detached (it cuts the lanes an agent's shell may depend on) and read its
# log afterwards:  nohup scripts/fleet/cut-daemon.sh >/tmp/cut.log 2>&1 &
set -u
LABEL=cortexkit.subc
DOMAIN="gui/$(id -u)"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/.local/share/cortexkit/run/logs"
stamp() { date -u +%H:%M:%SZ; }
job_pid() { launchctl print "$DOMAIN/$LABEL" 2>/dev/null | awk '/^\tpid = /{print $3; exit}'; }
job_loaded() { launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1; }
refuse() { echo "$(stamp) REFUSED: $*" >&2; exit 2; }

[ -f "$PLIST" ] || { echo "$(stamp) REFUSED: no plist at $PLIST"; exit 2; }
if [ "${1:-}" = --dry-run ]; then
  old_pid=$(job_pid)
  echo "$(stamp) dry run: would cut $DOMAIN/$LABEL (outgoing pid: ${old_pid:-none}) using $PLIST"
  exit 0
fi

command -v ck >/dev/null || refuse "ck is required to check stack status and the current window"
if status=$(ck stack status --json); then
  marker=$(printf '%s' "$status" | python3 -c '
import json, sys

def unique_keys(pairs):
    result = dict(pairs)
    if len(result) != len(pairs):
        raise ValueError("duplicate JSON key")
    return result

def invalid_constant(value):
    raise ValueError(f"invalid JSON constant {value}")

s = json.load(sys.stdin, object_pairs_hook=unique_keys, parse_constant=invalid_constant)
if s["schema"] != "ck.stack.status/v1" or s["stopped_marker"] not in ("absent", "stale", "present", "unknown"):
    raise SystemExit(1)
print(s["stopped_marker"])
' 2>/dev/null) || refuse "ck stack status --json did not return a parseable ck.stack.status/v1 report"
else
  refuse "ck stack status --json failed (exit $?)"
fi
case "$marker" in
  absent|stale) ;;
  *) refuse "ck stack status --json reports stopped_marker=$marker; not cutting the daemon" ;;
esac

if window=$(ck window current); then
  [[ "$window" =~ ^[0-9]{8}-[0-9]{6}-[0-9a-fA-F]{4}$ ]] \
    || refuse "ck window current did not return a window id"
  if ! step=$(ck window record activation --target daemon --window "$window" --owner-pid "$$") \
    || ! [[ "$step" =~ ^[1-9][0-9]*$ ]]; then
    refuse "could not record daemon activation in window $window (steps recorded: none)"
  fi
else
  window_status=$?
  [ "$window_status" -eq 3 ] || refuse "ck window current failed (exit $window_status)"
  window=""
fi

old_pid=$(job_pid)
echo "$(stamp) outgoing daemon pid: ${old_pid:-none}"
cut_at=$(date -u +%Y-%m-%dT%H:%M:%S)

# An older daemon may not recognize the announcement; the cut must still run.
ck daemon announce-restart || true
launchctl bootout "$DOMAIN/$LABEL" 2>&1 | sed "s/^/$(stamp) bootout: /"

# Wait for the old process AND the job registration to be gone. The daemon's
# own shutdown is bounded (module deadlines are capped at 25 s, plist
# ExitTimeOut is 35 s), so 60 s is generous; past that, go on and let
# bootstrap's retries decide.
for _ in $(seq 1 120); do
  alive=0
  [ -n "${old_pid:-}" ] && kill -0 "$old_pid" 2>/dev/null && alive=1
  if [ $alive = 0 ] && ! job_loaded; then break; fi
  sleep 0.5
done
if [ -z "$window" ]; then
  echo "$(stamp) outgoing daemon gone (pid alive: $alive, job loaded: $(job_loaded && echo yes || echo no))"
else
  echo "$(stamp) outgoing job loaded: $(job_loaded && echo yes || echo no)"
fi

# A pid alone cannot distinguish a reused pid from the previous daemon. When
# the restart is journaled, ck window done verifies that the daemon is new.
pid_ready() { [ -n "${new_pid:-}" ] && { [ -n "$window" ] || [ "$new_pid" != "${old_pid:-}" ]; }; }

ok=0
for attempt in 1 2 3 4 5 6 7 8 9 10; do
  if job_loaded; then
    # KeepAlive may already have relaunched it; that counts.
    new_pid=$(job_pid)
    if pid_ready; then ok=1; break; fi
  fi
  out=$(launchctl bootstrap "$DOMAIN" "$PLIST" 2>&1) && { ok=1; echo "$(stamp) bootstrap ok (attempt $attempt)"; break; }
  echo "$(stamp) bootstrap attempt $attempt failed: $out"
  sleep 3
done
[ $ok = 1 ] || { echo "$(stamp) FAILED: could not bootstrap $LABEL; the daemon is DOWN. Run: launchctl bootstrap $DOMAIN $PLIST"; exit 1; }

for _ in $(seq 1 60); do
  new_pid=$(job_pid)
  if pid_ready; then break; fi
  sleep 0.5
done
pid_ready || { echo "$(stamp) FAILED: no new daemon pid after bootstrap"; exit 1; }

log="$LOG_DIR/subc.$(date -u +%F).log"
for _ in $(seq 1 60); do
  started=$(awk -v t="$cut_at" 'substr($1,1,19) >= t && /subc daemon starting/' "$log" 2>/dev/null | tail -1)
  [ -n "$started" ] && break
  sleep 0.5
done
[ -n "$started" ] || { echo "$(stamp) FAILED: pid $new_pid is running but no 'subc daemon starting' line after $cut_at in $log"; exit 1; }
if [ -n "$window" ] && ! ck window 'done' "$step" --window "$window" --owner-pid "$$" >/dev/null; then
  echo "$(stamp) FAILED: could not mark window $window step $step done" >&2
  exit 1
fi
echo "$(stamp) NEW daemon pid $new_pid: ${started:0:24}"
echo "$(stamp) verify: inode proc == disk, and 'ck module list' shows every module running"
