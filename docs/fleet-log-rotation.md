# Fleet log rotation

## It is one problem, not fourteen

Measured before designing. Twelve of the fourteen supervised modules **write no
log files at all** — they write to standard output, the daemon's children inherit
its descriptors, and the process launcher redirects the lot into a single file.
That file is the fleet's combined output and it is currently **1.5 GB**, growing
about 22 MB a day since it was created a month ago.

So "every module adopts a rotation library" is the wrong shape: thirteen of them
have nothing to adopt it into. **The stream belongs to the supervisor, and so
does the rotation.**

One exception: the code-search module writes its own per-process files — 96 of
them, 145 MB. That needs an age-based sweep rather than rotation, and its owner
already runs sweeps of that kind for other state.

## Why the obvious fix corrupts the file

The tempting move is to truncate in place and let writers continue. Measured why
that fails here: **fifteen processes hold that descriptor** — the daemon plus
every child that inherited it — and each carries its own write offset, currently
at 1.5 GB.

Truncating frees the blocks but does not move anyone's offset. The next write
lands at 1.5 GB again and the filesystem fills the gap with a hole. The result is
a sparse file that reports its old size, grows from where it left off, and is
now full of unreadable zeroes. **Disk is reclaimed once and the problem returns
worse**, because the file no longer starts at a real record.

Rename-and-signal has the mirror problem: renaming moves the name, not the open
descriptor, so all fifteen processes keep writing into the now-nameless file. It
only works if every writer reopens, and twelve of them have no code that could.

## The shape that works

The supervisor stops handing children its own output descriptor and gives each a
pipe instead, then reads those pipes and writes through **one rotating writer it
owns**. Rotation becomes an internal operation on a file with a single writer:
close, rename, reopen, no signalling and no cooperation from anyone.

Two things fall out of it that are worth as much as the rotation:

**Attribution.** Today every line lands in one file with no reliable marker for
which module produced it — a sample of recent output shows entries from at least
three subsystems in different formats, and one cannot be attributed at all. A
supervisor reading distinct pipes knows the source of every line by construction.

**Backpressure is explicit.** A child writing faster than the reader drains
blocks on its pipe, which is visible and bounded. Today it writes directly to a
file that grows without limit.

The cost is real: this is a change to how children are spawned, and it puts the
supervisor on the path of every log line. It wants its own window and its own
rollback, not a corner of a restart.

## Interim

Nothing here is urgent — 22 MB a day against 118 GB free. The file can be rotated
safely at any point when the daemon is down, since no descriptors are open: move
it aside and let the launcher recreate it on start. **A restart window is exactly
that moment**, so the interim fix costs nothing extra if taken during one.
