# Phone effect store on SQLite, and list-based reconciliation

Status: draft r2 (SUBC). r2 folds CKIOS's review of section 1 and CALLO's review of
section 2.
Nothing here is built. The contract it must keep is `docs/subc-federation-design.md`
section 6.1: effect id `(origin pubkey, incarnation, seq)`, intent durable before the
first network write, outcome durable before the caller sees the reply, recovery
reconciliation, and no change ever executed twice.

## Why

The phone keeps its send log as one JSON document (`fed-state.json`) rewritten whole on
every step, with its own write/flush/rename/flush-folder sequence. That shape assumed a
handful of open changes. Settled records were never deleted, so on the operator's phone
it reached about 540 records and 3 MB, and each change costs several full flushes. The
8.7 s pause was a separate bug (fixed in 7f343c14); the store shape is still wrong for
what it does. Callosum's side of the same ledger is already SQLite (`dedup_ledger`,
keyed by peer, incarnation, seq).

## 1. Phone store (SubcFed, Swift, system SQLite via `import SQLite3`)

No new dependency. Not Core Data or SwiftData: the send log needs exact control over
when a write is durable.

Tables:
- `meta(key PRIMARY KEY, value)`: schema version, local identity digest, local
  incarnation, local ledger epoch, next seq.
- `destination(responder_fp PRIMARY KEY, responder_pubkey, observed_peer_incarnation,
  observed_peer_ledger_epoch, confirmed_incarnation, confirmed_seq)`.
- `effect(responder_fp, incarnation, seq, phase, disposition, peer_ledger_epoch,
  peer_incarnation, terminal_kind, terminal_code, terminal_body,
  PRIMARY KEY (responder_fp, incarnation, seq))`, plus an index on
  `(responder_fp, disposition)` so "every open change for this Mac" is one indexed read.
- `poisoned_epoch(responder_fp, epoch, PRIMARY KEY (responder_fp, epoch))`.

Durability: WAL mode, `synchronous=FULL`, `fullfsync=ON` and `checkpoint_fullfsync=ON`
(Darwin SQLite otherwise uses plain fsync, weaker than today's F_FULLFSYNC). One commit
is one full flush of the WAL; a checkpoint, every thousand or so pages, adds flushes of
its own.

Transactions per change:
1. Reserve the seq and insert the intent row in one `BEGIN IMMEDIATE` transaction.
   Durable before the first network write. (1 flush)
2. `sent`: not durable. After a crash, `intent` and `sent` reconcile the same way (both
   ask the Mac). The tidy-up task is auditing every reader of `.sent`; this design adopts
   its finding.
3. Outcome, watermark advance and pruning in one transaction. Durable before the caller
   sees the reply. (1 flush)

So a change costs 2 full flushes, against 6 in the JSON file store after d8d0b9a8 (about
9-12 before it).

Queries that replace hand-written loops:
- Open changes: `SELECT ... WHERE responder_fp=? AND disposition='unknown'`.
- Confirmed watermark: the lowest open seq minus one, capped at the highest settled seq
  (`MIN`/`MAX` over the index), local incarnation only.
- Regression sentinel: the highest-seq `recorded` row for the live peer epoch.
- Pruning: delete settled rows of the local incarnation at or below the watermark,
  except the sentinel row for each peer epoch. Nothing is pruned while any epoch of that
  destination is poisoned.

Locking: SQLite's own locking replaces the advisory lock file. The database stays in
the app's own Application Support folder, never an app-group container: iOS terminates a
suspended app holding a lock on a shared-container file (0xdead10cc). CKIOS confirmed
that only the app process opens the store (the notification extension does not link
SubcFed; no widgets or background tasks). The app is woken in the background by silent
pushes and can write the store then, so:
- Rule: no transaction stays open across an `await` or a possible suspension. Every
  transaction is short and synchronous; no long-lived `BEGIN`, no read transaction held
  across an await.

File protection and backup:
- Files keep the default protection class (complete until first user authentication).
  The database, `-wal` and `-shm` must all carry the same class.
- Before first unlock an open fails with an I/O error. That is a distinct, retryable
  "store locked" failure, never treated as "no database".
- The database, `-wal` and `-shm` are excluded from backup (`isExcludedFromBackup`). The
  device key is Keychain `AfterFirstUnlockThisDeviceOnly` (FedPrivateKeyStore.swift:145),
  so a restored phone has a new identity. A restored send log would belong to an identity
  it no longer has, and could carry an old incarnation. A fresh store mints a fresh
  incarnation, which is what section 6.1 relies on.

Migration: on first open, if `fed-state.json` exists and the database file does not
exist, import it in one transaction. "Does not exist" is a file-existence check, never an
open failure: a locked phone that cannot open an existing database must not re-import
the JSON over it. For that check to be sound the database file must never exist
half-built: a new database (imported or fresh) is built under a temporary name and
renamed into place once complete, so a crash during the import leaves no database and
the next open imports again. Then check that the imported document and the database
give the same open changes, watermark, sentinel per epoch, poisoned epochs, reservation
state and incarnation, and only then rename the JSON to `fed-state.json.migrated`. Keep that file for one release. If the check fails,
refuse to open with a distinct `FedFailure` case of its own (the app shows a specific
notice for it, not "can't reach your Mac") and leave both files untouched.

## 2. Wire: ask about many changes at once, and confirm by id

Today `effect_status` takes one id, so reconnecting with N open changes is N round trips.
The watermark is one number on `call` and `keepalive` frames; callosum prunes rows at or
below it after 24 h of reachable time and moves its expired floor.

Proposed (settled with CALLO):

**List `effect_status`.**
- Up to 32 ids per request, the size of callosum's rate burst (32/s, burst 32, 16
  concurrent); a larger list could never be admitted. The bucket is charged per id, so a
  list cannot bypass the rate.
- `busy` applies to the whole reply, never per item: if the whole list cannot be charged,
  the answer is one `busy`. One ledger epoch per reply; epoch is still classified before
  busy.
- Reply: the header carries per item `{effect, status, ledger_complete, kind, body_len}`.
  The frame body concatenates the included outcome bodies in item order, within the
  negotiated body cap. An item whose body does not fit is marked `body_deferred: true`
  and the phone asks for it again on its own. This is distinct from today's
  `body_omitted`, which means the body is over the cap and will never come.
- The phone's reconnect becomes one query for its open changes plus the sentinel, then
  ceil(n/32) round trips plus any deferred bodies.

**Confirm by id (`confirmed_effects`).** The phone names ids whose outcome it holds, so
a stuck change no longer holds back the Mac's cleanup of everything after it. The
watermark stays: it is still what moves callosum's expired floor.
- The rule this must not break: callosum never answers `not_found` with
  `ledger_complete: true` for an id that existed. The phone reads that as "provably never
  executed", and it asks about its regression sentinel, which is a recorded change.
- So confirmed ids are kept as ranges above the watermark in `ledger_meta`, per (peer,
  incarnation), not as tombstone rows: a stuck change holds the watermark forever, and
  tombstones would still count against the per-peer row cap and end in
  `fed_ledger_full`. Ranges cost one entry per gap. An id inside a range answers the new
  status `confirmed`. At most 64 ranges per (peer, incarnation); a confirmation that
  would exceed that is ignored and the rows stay, which is safe because confirmation is
  only an optimisation. A store rollback loses the ranges with the rows, so regression
  detection still fires.
- Confirmation applies only to rows with a recorded outcome. An admitted row with no
  outcome may still be running and its outcome write must find its row, so confirming it,
  an unknown id, or an id above the high-water mark is ignored.
- Deletion of a confirmed row waits for the same 24 h reachable-time grace the watermark
  uses: one retention rule.
- Re-sending a confirmed id is still refused by the existing high-water fence
  (`fed_seq_fenced`), never executed.

Rollout, readers first: callosum ships list `effect_status`, `confirmed_effects` and the
`confirmed` status, advertised as the hello capability `effects-v2`. The phone uses them
only when the Mac advertises it, and keeps today's per-id queries and watermark
otherwise.

## Work split

1. Callosum, `effects-v2` reader (CALLO): list `effect_status`, confirmed ranges, the
   `confirmed` status. Placed and running before any phone sends v2.
2. SubcFed store on SQLite with migration (SUBC): section 1. Ships with the v1 wire
   first, so the store change and the wire change are verified separately.
3. SubcFed `effects-v2` sender (SUBC): list reconnect and `confirmed_effects`, used only
   when the Mac advertises the capability.
4. App (CKIOS): the saved-state size readout reads the database and `-wal`; the new
   migration failure gets its own notice. iOS floor is 26.0; system SQLite there has WAL
   and the fullfsync pragmas.
