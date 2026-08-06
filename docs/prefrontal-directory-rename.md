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

## What is still unknown

The code-search module keeps 1957 per-root cache directories under 16-character
hashed names. **Whether that name derives from the absolute path is unresolved** —
md5 and truncated sha256 of the path both fail to match, so the mapping is
something else. Its owner has the question.

Two outcomes: if the key is path-derived, the rename orphans that root's index
and callgraph and the next attach pays a cold rebuild — acceptable if expected.
If it derives from a repo identity, the rename costs nothing there.

The answer that would matter most is a third one: **anything that goes stale
rather than simply missing.** A cold miss is visible and self-correcting; a stale
hit is the shape that had a module serving an eleven-day-old build while every
path-derived check reported it current.

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
