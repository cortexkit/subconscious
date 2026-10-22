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
public `FedStateStoreInUse` error. This is not a new `FedFailure` case. A share
extension should defer its operation or ask the app to relinquish ownership,
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
