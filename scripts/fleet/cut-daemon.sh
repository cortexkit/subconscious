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
# So this script waits for the old job to be gone, retries bootstrap, and only
# reports success after reading a new pid and the new daemon's start line.
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

[ -f "$PLIST" ] || { echo "$(stamp) REFUSED: no plist at $PLIST"; exit 2; }
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
echo "$(stamp) outgoing daemon gone (pid alive: $alive, job loaded: $(job_loaded && echo yes || echo no))"

ok=0
for attempt in 1 2 3 4 5 6 7 8 9 10; do
  if job_loaded; then
    # KeepAlive may already have relaunched it; that counts.
    new_pid=$(job_pid)
    if [ -n "${new_pid:-}" ] && [ "$new_pid" != "${old_pid:-}" ]; then ok=1; break; fi
  fi
  out=$(launchctl bootstrap "$DOMAIN" "$PLIST" 2>&1) && { ok=1; echo "$(stamp) bootstrap ok (attempt $attempt)"; break; }
  echo "$(stamp) bootstrap attempt $attempt failed: $out"
  sleep 3
done
[ $ok = 1 ] || { echo "$(stamp) FAILED: could not bootstrap $LABEL; the daemon is DOWN. Run: launchctl bootstrap $DOMAIN $PLIST"; exit 1; }

for _ in $(seq 1 60); do
  new_pid=$(job_pid)
  if [ -n "${new_pid:-}" ] && [ "$new_pid" != "${old_pid:-}" ]; then break; fi
  sleep 0.5
done
[ -n "${new_pid:-}" ] && [ "$new_pid" != "${old_pid:-}" ] || { echo "$(stamp) FAILED: no new daemon pid after bootstrap"; exit 1; }

log="$LOG_DIR/subc.$(date -u +%F).log"
for _ in $(seq 1 60); do
  started=$(awk -v t="$cut_at" 'substr($1,1,19) >= t && /subc daemon starting/' "$log" 2>/dev/null | tail -1)
  [ -n "$started" ] && break
  sleep 0.5
done
[ -n "$started" ] || { echo "$(stamp) FAILED: pid $new_pid is running but no 'subc daemon starting' line after $cut_at in $log"; exit 1; }
echo "$(stamp) NEW daemon pid $new_pid: ${started:0:24}"
echo "$(stamp) verify: inode proc == disk, and 'ck module list' shows every module running"
