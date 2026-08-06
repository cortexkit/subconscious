#!/usr/bin/env fish
#
# Renames the working directory alfonso -> prefrontal and repoints the session
# state that refers to it.
#
# Run this with the fleet DOWN. Two reasons, both measured rather than assumed:
# the code-search module records a checkout path and a process id in its artifact
# lease, so renaming under a live daemon makes the moved checkout look like it is
# owned by another process; and it kills background tasks once a root is
# confirmed absent, which a rename produces while the work is still running fine
# through its open directory handle.
#
# Safe to run twice. Every step checks whether it has already been done.

set -l OLD "$HOME/Work/Projects/CortexKit/alfonso"
set -l NEW "$HOME/Work/Projects/CortexKit/prefrontal"
set -l OCDB "$HOME/.local/share/opencode/opencode.db"
set -l MCDB "$HOME/.local/share/cortexkit/magic-context/store.db"
set -l STAMP (date -u +%Y%m%dT%H%M%SZ)
set -l BACKUPS "$HOME/.local/share/cortexkit/backups/rename-$STAMP"

function say;   set_color cyan;   echo "$argv"; set_color normal; end
function ok;    set_color green;  echo "  ok: $argv"; set_color normal; end
function warn;  set_color yellow; echo "  !! $argv"; set_color normal; end
function fail;  set_color red;    echo "  FAIL: $argv"; set_color normal; exit 1; end

say "== preflight =="

# The destination must not already exist as something else. If BOTH exist we
# cannot tell a half-finished rename from two unrelated directories, so refuse
# rather than guess.
if test -d "$OLD" -a -d "$NEW"
    fail "both $OLD and $NEW exist -- resolve by hand, this script will not choose"
end
if test ! -d "$OLD" -a -d "$NEW"
    ok "directory already renamed"
else if test ! -d "$OLD"
    fail "neither directory exists -- wrong host or wrong path?"
end

# The fleet must be down. A live daemon is the condition that makes the rename
# unsafe, so this is a refusal rather than a warning.
set -l daemon (ps -Ao pid=,comm= | awk -v p="$HOME/.local/share/cortexkit/bin/ck-subc" '$2==p{print $1; exit}')
if test -n "$daemon"
    fail "the subc daemon is running (pid $daemon) -- stop the fleet first"
end
ok "fleet is down"

# Anything holding the directory keeps working through its open handle and will
# be reaped later as if the root had been deleted. Report rather than refuse:
# language servers are expected here and are harmless to kill.
set -l holders (lsof -a -d cwd -- "$OLD" 2>/dev/null | tail -n +2 | wc -l | string trim)
if test "$holders" -gt 0
    warn "$holders process(es) have their working directory inside $OLD"
    lsof -a -d cwd -- "$OLD" 2>/dev/null | tail -n +2 | awk '{printf "     %-18s pid %s\n", $1, $2}'
    warn "they will keep running against a path that no longer exists"
else
    ok "nothing is working inside the directory"
end

# A live session in that directory means an editor is open on it. The rename
# would succeed and the editor would be pointed at nothing.
if test -f "$OCDB"
    set -l live (sqlite3 "$OCDB" "select count(*) from session where directory='$OLD' and time_updated > (strftime('%s','now')-3600)*1000;" 2>/dev/null)
    if test "$live" -gt 0
        fail "$live session(s) were active in that directory within the hour -- close them first"
    end
    ok "no recently active sessions in the directory"
end

say ""
say "== backups =="
mkdir -p "$BACKUPS"; chmod 700 "$BACKUPS"

# Copy with the restrictive mode set AT CREATION rather than after. A copy
# inherits its source's permissions, and nothing ever opens a backup, so a
# fix that applies on open can never reach it.
for db in "$OCDB" "$MCDB"
    if test -f "$db"
        set -l name (basename (dirname "$db"))"-"(basename "$db")
        cp -p "$db" "$BACKUPS/$name"; chmod 600 "$BACKUPS/$name"
        ok "backed up $name ("(du -h "$BACKUPS/$name" | cut -f1 | string trim)")"
    end
end
say "  backups in $BACKUPS"

say ""
say "== rename =="
if test -d "$OLD"
    mv "$OLD" "$NEW"; or fail "mv failed"
    ok "$OLD -> $NEW"
else
    ok "already renamed, nothing to move"
end

say ""
say "== session state =="

# Only two things resolve this path at runtime: the project row, and each
# session's working directory.
#
# The same text also appears roughly 76,000 times inside stored messages and
# tool-call records. THOSE ARE DELIBERATELY LEFT ALONE. They are the record of
# calls that happened, against paths that existed at the time; nothing resolves
# them and they are only ever displayed. Rewriting them would make the history
# assert something that was never true.
if test -f "$OCDB"
    set -l before_p (sqlite3 "$OCDB" "select count(*) from project where worktree='$OLD';")
    set -l before_s (sqlite3 "$OCDB" "select count(*) from session where directory='$OLD';")
    say "  rows pointing at the old path: $before_p project, $before_s session"

    sqlite3 "$OCDB" "update project set worktree='$NEW' where worktree='$OLD';
                     update session set directory='$NEW' where directory='$OLD';"
    or fail "opencode update failed -- restore from $BACKUPS"

    set -l after_old (sqlite3 "$OCDB" "select (select count(*) from project where worktree='$OLD') + (select count(*) from session where directory='$OLD');")
    set -l after_new (sqlite3 "$OCDB" "select (select count(*) from project where worktree='$NEW') + (select count(*) from session where directory='$NEW');")
    test "$after_old" -eq 0; or fail "$after_old rows still point at the old path"
    ok "moved (expected "(math $before_p + $before_s)", now at new path: $after_new)"
end

# This table was empty for the old path when measured, so expect zero. Run it
# anyway: zero rows now does not mean zero rows at the moment you run this.
if test -f "$MCDB"
    set -l n (sqlite3 "$MCDB" "select count(*) from mc_transform_session_roots where project_root='$OLD';" 2>/dev/null)
    if test "$n" -gt 0
        sqlite3 "$MCDB" "update mc_transform_session_roots set project_root='$NEW' where project_root='$OLD';"
        or fail "context store update failed -- restore from $BACKUPS"
        ok "context store: moved $n row(s)"
    else
        ok "context store: nothing referenced the old path"
    end
end

say ""
say "== verify =="
test -d "$NEW"; and ok "directory exists at the new path"
test ! -d "$OLD"; and ok "old path is gone"

# Confirm it is still the repository we think it is, rather than merely a
# directory with the right name.
set -l remote (git -C "$NEW" remote get-url origin 2>/dev/null)
if test -n "$remote"
    ok "git remote: $remote"
end

say ""
say "Done. The code-search caches survive this: they are keyed on the repository's"
say "root commits rather than its path, so the index and callgraph carry over. Only"
say "the per-checkout health caches rebuild, in the background, in minutes."
say ""
say "Restore, if needed:  cp $BACKUPS/* back over the databases and mv the directory back."
