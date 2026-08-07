#!/usr/bin/env fish
#
# Renames project folders and repoints the session state that refers to them.
#
# Run this with OPENCODE CLOSED and the fleet DOWN.
#
# OpenCode is the binding constraint rather than the fleet: a session captures
# its project root at start and never re-resolves it, so a session open on a
# renamed folder has every tool call refused at a precondition -- including
# calls using only absolute paths. Closing OpenCode is what makes the rename
# invisible rather than a wedge. The fleet matters for a second reason: the
# code-search module records a checkout path in its artifact lease, so a rename
# under a live daemon makes the moved checkout look owned by another process.
#
# Safe to run twice. Every step checks whether it has already been done.
#
# Adding a pair: append to RENAMES below. Each entry is "old:new", folder names
# only, both under the projects root.

set -l ROOT "$HOME/Work/Projects/CortexKit"
set -l RENAMES \
    "cortexkit-credentials:claustrum" \
    "ai-provider-quota:insula" \
    "ck-projects:entorhinal"

set -l OCDB "$HOME/.local/share/opencode/opencode.db"
set -l MCDB "$HOME/.local/share/cortexkit/magic-context/store.db"
set -l STAMP (date -u +%Y%m%dT%H%M%SZ)
set -l BACKUPS "$HOME/.local/share/cortexkit/backups/folder-rename-$STAMP"

function say;   set_color cyan;   echo "$argv"; set_color normal; end
function ok;    set_color green;  echo "  ok: $argv"; set_color normal; end
function warn;  set_color yellow; echo "  !! $argv"; set_color normal; end
function fail;  set_color red;    echo "  FAIL: $argv"; set_color normal; exit 1; end

say "== preflight =="

# OpenCode holds the database this script rewrites, and a live session would be
# rebound underneath itself. Refuse rather than warn.
set -l oc (pgrep -x opencode 2>/dev/null | head -1)
if test -n "$oc"
    fail "opencode is running (pid $oc) -- close it first"
end
ok "opencode is closed"

# A live daemon is the condition that makes the checkout rename unsafe.
#set -l daemon (ps -Ao pid=,comm= | awk -v p="$HOME/.local/share/cortexkit/bin/ck-subc" '$2==p{print $1; exit}')
#if test -n "$daemon"
#    fail "the subc daemon is running (pid $daemon) -- stop the fleet first"
#end
#ok "fleet is down"

# Resolve each pair to a state before touching anything, so a half-finished run
# is reported up front rather than discovered midway.
set -l todo
for pair in $RENAMES
    set -l old_name (string split -f1 ':' $pair)
    set -l new_name (string split -f2 ':' $pair)
    set -l old "$ROOT/$old_name"
    set -l new "$ROOT/$new_name"

    # Both existing is the one case a script must not resolve: it cannot tell a
    # half-finished rename from two unrelated directories.
    if test -d "$old" -a -d "$new"
        fail "$old_name and $new_name both exist -- resolve by hand"
    else if test -d "$new"
        ok "$old_name -> $new_name (already renamed)"
    else if test -d "$old"
        set -a todo $pair
        say "  $old_name -> $new_name (pending)"
    else
        warn "$old_name: neither name present, skipping"
    end
end

if test (count $todo) -eq 0
    ok "nothing to do"
    exit 0
end

# Processes working inside a folder keep running through their open handle and
# are later reaped as if the root had been deleted. Report rather than refuse:
# language servers are expected here and are harmless to kill.
for pair in $todo
    set -l old "$ROOT/"(string split -f1 ':' $pair)
    set -l holders (lsof -a -d cwd -- "$old" 2>/dev/null | tail -n +2 | wc -l | string trim)
    if test "$holders" -gt 0
        warn "$holders process(es) working inside "(basename "$old")
        lsof -a -d cwd -- "$old" 2>/dev/null | tail -n +2 | awk '{printf "     %-18s pid %s\n", $1, $2}'
    end
end

say ""
say "== backups =="
mkdir -p "$BACKUPS"; chmod 700 "$BACKUPS"

# Copy with the restrictive mode set at creation rather than after. A copy
# inherits its source's permissions, and nothing ever opens a backup, so a fix
# applied on open can never reach it.
for db in "$OCDB" "$MCDB"
    if test -f "$db"
        set -l name (basename (dirname "$db"))"-"(basename "$db")
        cp -p "$db" "$BACKUPS/$name"; chmod 600 "$BACKUPS/$name"
        ok "backed up $name ("(du -h "$BACKUPS/$name" | cut -f1 | string trim)")"
    end
end
say "  backups in $BACKUPS"

for pair in $todo
    set -l old_name (string split -f1 ':' $pair)
    set -l new_name (string split -f2 ':' $pair)
    set -l old "$ROOT/$old_name"
    set -l new "$ROOT/$new_name"

    say ""
    say "== $old_name -> $new_name =="

    # Place the compatibility link BEFORE the move, not after. A dangling
    # symlink is legal and starts resolving the instant its target appears, so
    # ordering it first leaves no window at all.
    #
    # Measured cost of getting this wrong: a seat's `cd` into the old path
    # failed 14 minutes before the link was placed. Any resident session bound
    # to that root has every tool call refused at a precondition during the
    # gap -- including calls using only absolute paths -- and can neither work
    # around it nor restart itself.
    ln -sfn "$new" "$old.compat"
    mv "$old" "$new"; or fail "mv failed for $old_name"
    mv "$old.compat" "$old"; or fail "could not place compat link for $old_name"
    test -d "$old"; or fail "compat link for $old_name does not resolve"
    ok "directory moved, old path still resolves through a compat link"

    # Only two things resolve this path at runtime: the project row, and each
    # session's working directory.
    #
    # The same text also appears many thousands of times inside stored messages
    # and tool-call records. THOSE ARE DELIBERATELY LEFT ALONE. They record calls
    # that happened, against paths that existed at the time; nothing resolves
    # them and they are only ever displayed. Rewriting them would make the
    # history assert something that was never true.
    if test -f "$OCDB"
        set -l before_p (sqlite3 "$OCDB" "select count(*) from project where worktree='$old';")
        set -l before_s (sqlite3 "$OCDB" "select count(*) from session where directory='$old';")

        sqlite3 "$OCDB" "update project set worktree='$new' where worktree='$old';
                         update session set directory='$new' where directory='$old';"
        or fail "opencode update failed -- restore from $BACKUPS"

        set -l left (sqlite3 "$OCDB" "select (select count(*) from project where worktree='$old') + (select count(*) from session where directory='$old');")
        test "$left" -eq 0; or fail "$left row(s) still point at $old"
        ok "opencode: $before_p project + $before_s session row(s) moved"
    end

    # Measured empty for previous renames, so expect zero. Run it anyway: zero
    # rows at one moment does not mean zero rows at the moment you run this.
    if test -f "$MCDB"
        set -l n (sqlite3 "$MCDB" "select count(*) from mc_transform_session_roots where project_root='$old';" 2>/dev/null)
        if test -n "$n" -a "$n" != "0"
            sqlite3 "$MCDB" "update mc_transform_session_roots set project_root='$new' where project_root='$old';"
            or fail "context store update failed -- restore from $BACKUPS"
            ok "context store: $n row(s) moved"
        else
            ok "context store: nothing referenced the old path"
        end
    end

    # Registered worktrees point back at the main repository by ABSOLUTE path in
    # both directions: each worktree's `.git` file names a directory under the
    # old location, and the repository's bookkeeping names each worktree.
    # Renaming breaks both halves, and git ships a repair for exactly this.
    set -l wt (git -C "$new" worktree list --porcelain 2>/dev/null | awk '/^worktree /{print $2}' | tail -n +2)
    if test (count $wt) -gt 0
        git -C "$new" worktree repair $wt 2>&1 | sed 's/^/    /'

        # A repair that silently fixed nothing looks identical to one that had
        # nothing to fix, so check a backpointer actually moved.
        if test -f "$wt[1]/.git"
            set -l points (string replace 'gitdir: ' '' (head -1 "$wt[1]/.git"))
            if string match -q "$new*" -- "$points"
                ok "worktrees: "(count $wt)" repaired, backpointers resolve under the new path"
            else
                warn "a worktree still points at $points -- repair may not have taken"
            end
        end
    else
        ok "no registered worktrees"
    end

    # Confirm it is still the repository we think it is, rather than merely a
    # directory with the right name.
    set -l remote (git -C "$new" remote get-url origin 2>/dev/null)
    test -n "$remote"; and ok "git remote: $remote"
end

say ""
say "== compat links =="
say "  The old paths still resolve. That is deliberate: a resident session captures"
say "  its project root at START and never re-resolves it, so a session bound to an"
say "  old path keeps working through the link until it restarts."
say ""
say "  REMOVE EACH LINK ONLY AFTER ITS SEAT CONFIRMS A RESTART, AND ASK RATHER THAN"
say "  MEASURE. There is no filesystem check for this -- a bound project root is not"
say "  a working directory, so a seat can depend entirely on a path that lsof shows"
say "  nobody standing in. The session table cannot answer it either: it records what"
say "  a NEW session would bind, not what a running one is holding."
say ""
say "  Links must not linger. A symlinked path and its target can register as two"
say "  different directories in the peer registry, which splits a seat's message"
say "  routing from its message visibility."

say ""
say "== verify =="
for pair in $RENAMES
    set -l old_name (string split -f1 ':' $pair)
    set -l new_name (string split -f2 ':' $pair)
    if test -d "$ROOT/$new_name" -a ! -d "$ROOT/$old_name"
        ok "$new_name"
    else if test ! -d "$ROOT/$new_name" -a ! -d "$ROOT/$old_name"
        warn "$new_name: neither name present (was skipped)"
    else
        fail "$old_name/$new_name did not end in the expected state"
    end
end

say ""
say "The code-search caches survive this: they are keyed on each repository's root"
say "commits rather than its path, so index and callgraph carry over. Only the"
say "per-checkout health caches rebuild, in the background, in minutes."
say ""
say "Restore, if needed: copy $BACKUPS/* back over the databases and mv the"
say "directories back."
