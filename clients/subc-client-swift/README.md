# SubcClient Swift package

Native client libraries for macOS 13+ and iOS 16+: `SubcClient` implements the
outer wire protocol, `SubcFed` implements fed sessions, and
`SubcChatAskSupport` provides shared surface models.

## Sharing a durable fed store with an app extension

Use one `FedSQLiteStateStore(directoryURL:)` in an App Group container for a
local identity. Within a process, pass **the same store actor** to all
`SubcFedClient` instances (for example, one client per paired computer). Do not
give each client or process its own independent store directory. Each new
database starts a new incarnation: a fresh numbering for the device's
mutations. The computer keeps a record of every mutation it receives until
the device confirms it saw the outcome, and those records count against a
limit per device key. A store that is thrown away never confirms its records,
so they pile up until that limit refuses the device's mutations. Successive
processes create their own actors over the same directory, only after the
previous owner has closed its actor.

`open(localPublicKey:)` acquires an exclusive, non-blocking `flock` on
`fed-state.lock` beside `fed-state.sqlite`, before opening, creating, cleaning
up, migrating or reading SQLite state. The lock file is created with mode 0600
and opened with `O_CLOEXEC | O_NOFOLLOW`. It remains locked until `close()` or
deinitialization. Do not delete or replace the lock file, even after close:
all participants must lock the same inode. SQLite transactions are short-lived,
but the store's ownership lock intentionally lasts across network awaits.

A second opener, including one in the same process, immediately throws the
public `FedStateStoreInUse` error. This is not a new `FedFailure` case. An
extension needing an ordinary session should ask the app to relinquish ownership,
then retry after the app has closed its store. It must not spin, wait while
holding an open store of its own, delete the lock/database, or mint a
replacement store and incarnation to work around contention.

### Shutdown order

Before giving another process ownership, or before iOS suspends the app:

1. Stop accepting new operations and end/finish or cancel active sessions and
   calls for **every** client sharing the store.
2. `await client.disconnect()` on **each** client, and stop all caller-owned
   tasks that could call those clients or the store again. Disconnect cancels
   that client's receive/subscription/background work and closes its transport;
   it does **not** close a shared store. Merely dropping a client is not a
   substitute for disconnect. Tear down every client sharing this store; do not
   reuse those clients when ownership returns.
3. `try await store.close()`. It closes SQLite first, discards transient sent
   markers, then releases the ownership lock and closes its descriptor. A
   successful close leaves no store-owned descriptor in the directory. It is
   idempotent; if SQLite close throws, ownership is retained, not handed off.

Do not rely on `deinit` before suspension: clients strongly retain the store,
and iOS can terminate an App Group owner suspended with a file lock
(`0xdead10cc`). Explicit close does not require deallocating the actor.

Close is **terminal for that actor**. Every later open, read or write through
it throws `FedStateStoreClosed` without touching SQLite or the lock, even if a
different actor or process now owns the directory. Reads/writes before the
initial open also fail; they never implicitly open SQLite.

On return, create a **new store actor and new clients** for the same directory
and identity. The new actor reloads disk state, including the same incarnation,
sequence reservations, and destinations written by the other process. There is
no process-wide path cache. Clients cache their initial enrollment decision and
session state, so carrying an old client across a handoff is not supported.

If the shutdown order is violated, client operations check the SQLite store's
lifetime and throw `FedStateStoreClosed`, including cached catalog lookups and
new subscriptions. A pending management reply that cannot be persisted because
the store was closed also fails with that typed error, not a successful reply
or a network retry. On detecting closure the client disconnects, stops its
background work, and publishes idle; there is no new public connection-state
or `FedFailure` enum case. Nonthrowing status access and `disconnect()` remain
available for teardown. This is a fail-closed backstop, not coordination for
closing beneath active calls: an interrupted mutation still needs ledger
reconciliation by the next owner, never blind replay.

## Share sessions beside the app's session

Use `FedShareClient` for share-extension sends to a paired computer advertising
`share-v1`. It has its own Noise IK connection and never opens the origin SQLite
store, arbitrates an ordinary session, or opens a rendezvous control socket.
It dials **LAN, then public direct candidates only**, never relay. The app may
keep its ordinary session open. Refresh the snapshot below when the app learns
new authenticated feature facts or candidates, or changes its origin identity.
If the snapshot does not advertise `share-v1`, `send` returns
`report.notSupportedByComputer == true` ("not supported by this computer")
without dialing or sending a hello. This is distinct from a failed connection;
per-call outcomes still honor any earlier durable marks.

Adopt two library-owned files in the same App Group directory in both targets.
Read `share-v1` from `SubcFedClient.peerAdvertisedFeatures`, the computer's
own hello list, not from `negotiatedFeatures`: the phone's ordinary hello never
offers `share-v1`, so the negotiated intersection never contains it.

```swift
let directory = FedShareDirectory(directoryURL: appGroupURL)
let ledger = FedShareLedger(directoryURL: appGroupURL)

// In the app, using its already-open shared FedStateStore and its ready
// SubcFedClient (`fedClient`) for this computer:
try await directory.update(
    computers: [FedShareComputer(
        publicKey: pairedComputerKey,
        lanCandidates: learnedLAN,
        publicCandidates: learnedPublic,
        advertisedShareV1: fedClient.peerAdvertisedFeatures.contains("share-v1"))],
    ownPublicKey: phonePublicKey,
    stateStore: store)

// In the extension, using the same Keychain identity as the app:
let computer = try directory.entry(forComputerKey: pairedComputerKey)!
let client = FedShareClient(keyStore: keys, ledger: ledger,
                           helloPolicy: try FedHelloPolicy(deviceName: "Phone"))
let request = try FedShareRequest(module: "messages", body: managementRequestJSON)
// Persist request.shareID, module and exact body bytes before calling send.
let report = try await client.send([request], to: computer)
```

The directory owns its JSON format and writer; the app passes explicit paired
computers, not JSON. It atomically replaces the whole snapshot, skipping the
write if unchanged. Its `originIncarnation` and `originLedgerEpoch` are read
from `stateStore.snapshot().global.localIncarnation/localLedgerEpoch`: the
**phone's persisted origin values**, exactly those its ordinary hello sends.
The extension carries them in its hello with `session_kind: "share"` and no
`connection_attempt_id`. By contrast, the epoch pinned in each ledger mark is
the **computer's `ledger_epoch` from the authenticated share-session hello**.
Never substitute the directory's origin epoch for that serving epoch.

The shared wire fixtures live in `vectors/fed-share-v1/`, pinned to the
`share-v1-vectors-v1.0` release. Share tests check every hello, frame, code and
disposition row, plus the release's SHA-256 digests. An illegal `never_ran`
proof is reported as `fed_protocol_violation` with an unknown outcome, never
as permission to move a marked id to the outbox.

`send` waits for `share_ready`, sends at most eight calls, one outstanding at a
time, stops starting calls after 60 seconds, and closes with `fed_share_done`.
An outstanding call may finish after the lifetime. Excess/unattempted calls
return outbox only if unmarked. Results use new share-only types: sent (including
module errors and withheld outcomes), refused with `neverRan`, unknown (may
have run), discarded (superseded claim or moved), or outbox. Raw bye codes are
preserved on the report; none alone licenses fallback for a marked id.

Before any call bytes are written, the ledger durably syncs the claim and the
immutable mark (share transport, computer key, own key, request digest,
computer epoch). The mark stores only the SHA-256 `requestDigest` of the exact
encoded `share_call` bytes, not the request itself. A different mark throws
`FedShareError.markConflict` from the
ledger API; the session maps binding changes to unknown and **never resends**.
An already-marked id stays share-only after silent closes or stale discovery.
Only the newest claim's answer is recorded; old answers return discarded.
Keep the ledger across launches, upgrades, key changes and computer re-pairing;
do not delete it to retry an unknown id, and never mint a new id for that retry.

The ledger stores only outcome metadata (`disposition`, `kind`, `code`,
`neverRan`), never response bodies; the full body is returned in the session
report. Keep the exact pending request in the app's own pending record for
resends. Each ledger entry's `createdAtMs` comes from its UUIDv7 share id, not
from when a retry happens. Every writing transaction prunes entries more than
30 days old. Exactly 30 days is still retained.

**A pending share older than 30 days can only be shown as unknown (Edit or
Delete).** It must not travel as a share or move to the outbox, even if its id
has no ledger entry: absence may mean its mark was pruned, not that it never
ran. `FedShareClient` returns unknown without dialing for these ids; the raw
ledger `claim` and `markIfAbsent` APIs throw `FedShareExpiredID`, and its outbox
check refuses them. Editing creates a deliberately new operation and id, not
an automatic retry of the old operation.

The ledger uses one compact version-1 format. Missing required fields,
unexpected fields and unsupported versions are storage errors; no alternate
format is converted or treated as an empty ledger.

For an outbox move, obtain a fresh share answer **outside the lock**. Then
recheck and perform the synchronous outbox file write and move under the same
ledger lock (using the returned call's id and claim):

```swift
let moved = try ledger.withLedgerLock { transaction -> Bool in
    guard try transaction.mayMoveToOutbox(result.shareID, claim: result.claim) else {
        return false
    }
    try writeOutboxEntrySynchronouslyAndDurably(result.shareID)
    try transaction.markMoved(result.shareID, claim: result.claim)
    return true
}
```

For a marked id this requires `never_ran: true` on the newest claim; a new
claim immediately revokes old permission. A moved id cannot be claimed again.
Make the outbox insertion idempotent by share id and recover an interrupted
outbox move before scheduling more share attempts: the outbox file and ledger
are separate durable commits. Never enqueue an unknown or discarded result.
`withLedgerLock<T>` is synchronous and returns its closure's value. Do not
retain the transaction or reenter the ledger from its closure. Every critical
section must be file I/O only: **no network calls, async closures or awaits**.
Use it only while the app/extension is active or holds a background task
assertion; iOS may kill a suspended App Group lock holder (`0xdead10cc`). The
per-transaction `flock` releases on process death. Never unlink or replace the
permanent `fed-share-ledger.lock` inode. Corrupt or unavailable files fail
closed; do not replace the ledger with an empty one to work around an error.

## Verification

On macOS with Xcode installed:

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift build
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer swift test
```

If the host already sandboxes the build and SwiftPM's nested sandbox is refused,
add `--disable-sandbox`. The cross-process store tests re-run a single XCTest
helper on macOS; the store itself also supports iOS. The package currently uses
Apple frameworks and is not a Linux package; its ownership lock uses Darwin's
`flock`, with a Glibc import branch for Linux ports.
