# Renaming the working directory to match the module

The repository is already `cortexkit/prefrontal` on the remote; only the local
directory still says `alfonso`. So this closes a gap rather than opening one.

**This is separate from the module-id rename** and should not ride the same
window. The id flip has a verified rollback story; a filesystem move gives a
failure two independent causes to untangle.

## What actually holds the literal path

Measured, not assumed. Counts are for the exact path, with sibling directories
(`alfonso-ios`, `alfonso-core`, `alfonso-routing`) excluded.

| Store | Occurrences | Load-bearing? |
|---|---|---|
| opencode `project.worktree` | 1 | **Yes** — the row that makes the directory a known project |
| opencode `session.directory` | 1065 | **Yes** — how a session finds its working directory |
| opencode `session.path` | 0 | — |
| opencode `message.data` | ~58k | **No** — recorded tool calls from past turns |
| opencode `part.data` | ~18k | **No** — same |
| magic-context session roots | 0 | — (table holds 7 rows overall, so the zero is real) |

**The 76,000 transcript occurrences must not be rewritten.** They are the record
of tool calls that happened, against paths that existed at the time. Rewriting
them would make the history assert something false — and they are never resolved,
only displayed.

That leaves **1066 rows of genuinely live state**, which is a small, bounded
update rather than the migration the raw count suggests.

## The code-search caches survive the rename

Resolved by their owner and verified here. The 16-character directory names are
keyed on **repository identity — the set of root commits — not the path**, which
is why hashing the path never matched. That repository has three root commits
(it was assembled from several histories), and both `index/be627d40119a995e` and
the matching callgraph files exist on disk.

So the search index, callgraph, and semantic embeddings **carry over untouched**:
renaming a directory does not change root commits. The path-to-key memo simply
misses under the new name, re-derives from git once, and lands on the same key.

This falls out of a property built for something else — repo-identity keying is
what lets temporary worktrees at different paths share one set of artifacts.
Rename-survival is a free consequence, which is better than a feature, because
nothing has to remember it applies.

Only the per-checkout code-health caches are path-keyed by design. They orphan
and rebuild in the background in minutes.

**Nothing goes stale** — path-keyed state misses cold rather than serving old
data under the new name.

### Two sequencing constraints, both from the same owner

**Rename with the module down.** Its artifact lease records the checkout path and
a process id. Rename under a live daemon and the renamed checkout looks like
another live process owning the artifact, so it comes up borrow-only until
reclaim. With the daemon stopped the recorded process is dead and reclaim is
clean.

**Rename with no background tasks running against that root.** A newly landed
behaviour kills surviving tasks once a root is confirmed absent — and a rename
makes the old path absent while running tasks keep working through their open
directory handles.

That second one is a category worth naming: not stale, not missing, but **a
correct absence detection firing on a path that moved rather than vanished.** The
mechanism is right; the event is ambiguous between two causes, and a rename is
indistinguishable from a deletion to anything that only checks presence.

Both constraints are satisfied by doing the move while the fleet is down — which
**changes the recommendation**: the filesystem move wants a window rather than
wanting to avoid one. Just not the same window as the module-id flip, whose
rollback story should not be entangled with a directory move.

## Sequence, once that is answered

1. Close the restart window first. One moving part at a time.
2. Stop the sessions using the directory (one was active within the hour).
3. Move the directory.
4. Update the two live tables: one project row, and the session rows pointing at
   the old path. Take a copy of the database first.
5. Leave transcript payloads untouched, deliberately.
6. Rebuild whatever caches turn out to be path-keyed, having decided in advance
   that a rebuild is the expected cost rather than a symptom.

## Why the two old directories still exist

`alfonso-core` and `alfonso-routing` are the **pre-consolidation standalone
repositories** — 241 and 25 commits, last touched in early July, and neither has
a remote. The consolidation did happen; these are what it left behind.

Their content lives on in the combined repository, but the original commits are
**not ancestors of it** — the history was rebuilt rather than merged, so those
commit identities exist only on this disk. That is why deleting them is a
question rather than housekeeping, and it is parked with the user.

Their build output (24 GB) has already been reclaimed; what remains is about
7 MB of source and history.
