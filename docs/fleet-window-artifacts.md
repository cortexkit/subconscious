# Window artifacts and discriminators

Per-module: which build ships, and how to prove the running process is that
build rather than its predecessor. Every marker below was measured on **both**
artifacts, and every control was measured too — a marker reading zero in old and
new alike looks like a clean before-state and proves nothing.

Owners supplied their own. Where an owner ruled a plausible check out, that is
recorded too: those are the ones that would have passed against a stale binary.

## Rejected discriminators

Collected first, because each is the obvious choice and each is wrong here.

| Module | Rejected | Why it fails |
|---|---|---|
| daemon, MCP gateway | version string | identical in both builds — mine, and I found it by measuring rather than assuming |
| credentials CLI | version string | 0.1.0 on both |
| federation | package version | never bumped |
| local inference | version string | 0.0.0 on both |
| connectors | catalog entry count | 17 before and after; one manifest is *replaced*, and a same-named file changed in place |
| broca | any new string literal | the change adds a match arm, and match-arm strings are compared by length and bytes without emitting contiguous constants |

The connectors case is the sharpest: last restage the count *worked*, by luck,
because that set happened to contain a manifest the old build rejected. A check
that passed for a reason that no longer holds is worse than one that never
worked, because nobody re-derives it.

## Per module

**daemon and MCP gateway** (mine): marker `preview` — 0 in the deployed
binaries, 3 in both new ones. It is the dry-run field added to the rescan
result, and it reaches the gateway through the shared control crate even though
that crate's own sources changed only in a test. Control: `route.open`, 5 in
every artifact.

**credentials**: exact SHA-256 equality against owner-supplied post-signing
hashes, with the installed hashes recorded as the differing control. Restarts
**separately, before everything else, and is excluded from the bounce** — the
owner's ruling: the vault is a dependency of several modules and gets its own
verification and its own rollback decision.

**connectors**: marker `schedulerAlive` (0 → 1), control `heartbeatAgeMs`
(1 → 1), both re-checked *after* signing. Runtime check is stronger: the health
metrics must carry `schedulerAlive` and `schedulerPassAgeMs`, which the deployed
build cannot emit. Catalog verified by its 17 **names**.

**federation**: marker `org_authority_floor` (0 → present), control
`dedup_ledger` (18 in the running binary).

**local inference**: prefer the capability check over any string — an operations
probe returning the decode lane, which a pre-decode build structurally cannot
produce. A capability discriminator cannot be satisfied by a stale binary that
happens to contain the right bytes.

**quota**: nothing to stage. Owner proved the gap inert by building both commits
with the embedded stamp pinned and comparing hashes — only equality is
conclusive, since a difference can be a comment shifting a panic line.

**backups, context**: artifacts pending; both owners are fixing defects found
during preparation.

## Staged

All of the below are in place at the deploy paths with the previous binary backed
up beside each. Staging is not a restart: the copy removes the destination first,
so every running process keeps executing its old file. Verified — for each, the
inode the live process is executing differs from the inode now on disk.

daemon, MCP gateway, backups, context, connectors (two binaries), and the two
executive binaries staged by their owner.

### Verifying a signed artifact

Signing rewrites the binary, so **a staged file can never match the source hash
its owner quoted**. That makes "verify by sha" unusable at the deploy path, and
an owner instructing it in good faith sends you to a check that cannot pass.

The substitute: **sign a reference copy of the verified source under the same
filename and compare.** Signing is deterministic — two signings of identical
bytes under the same name produce byte-identical output — so the comparison is
exact.

The filename is load-bearing and not obvious. macOS derives the signing
identifier from the file's name, so signing a copy called `ref` yields a
different result than signing the same bytes called `ck-engram`. A reference
signed under the wrong name reports a mismatch for a file that is correct, which
is how a good artifact gets rejected.

That mismatch also produced a wrong conclusion before it was chased down: signing
looked non-deterministic, when the variable was the name introduced by the check
itself. Two signings under a controlled name settled it.

A related asymmetry, since it nearly produced a wrong note: after signing, two of
three hashes were **unchanged** from what their owners quoted and one changed.
The two were already signed at source, so re-signing was a no-op. **A hash
mismatch after signing does not imply the wrong file, and a match does not imply
signing was skipped.**

## A moving branch tip is not a stale artifact

Three times during preparation an owner reported their tip had moved after giving
me a hash — a shell script, a test module, a comment. Each time the right action
was **do not rebuild**, and it is worth stating why, because the instinct runs the
other way.

A rebuild from effectively identical source produces **different bytes**: build
paths and timestamps land in the binary. So refreshing an artifact to match a
moved tip trades a delta you can reason about (a comment cannot reach a binary)
for a hash you must re-verify from nothing, and discards the provenance you
already established.

So the ledger records two commits per module where they differ: the one the
binary was **built from**, and the one that **merges**. Without that, a later
reader diffs the tip against the deployed binary, finds commits that never
shipped, and either re-verifies everything or concludes the deploy is stale.

## Acceptance

Per module, by its owner, and **every one includes a mutating call**. A
read-only route proves the service is serving and nothing about the write path.

Fleet-wide, one check nobody owns individually: every module's `store.db`, its
`-wal` and `-shm` siblings, and its `.lease` must read `-rw-------`. Nine must
flip; three are already correct and serve as the control proving the check can
read a correct state. The lease is the other half of the same fix and carries a
different risk — the store is confidentiality, the lease is the single-writer
fence, so writability there means a forgeable epoch.
