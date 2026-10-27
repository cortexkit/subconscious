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

## Recovering a mutation whose reply was lost

A mutating `callManagement` call whose reply is lost (the session drops after
the request went out) fails with `FedFailure.disconnected` or
`FedFailure.indeterminateMutation`: the phone cannot tell whether the module
applied it. The library does not resend it. On the next session,
reconciliation asks the computer what its ledger recorded for that mutation
and settles it from the answer. To get that answer back, send the mutation with
`callManagementTracked` and keep its effect id:

```swift
let target = try FedManagementTarget(moduleID: "example-module")
do {
    let result = try await client.callManagementTracked(
        target: target, method: "item.update", params: params)
    show(result.body)
} catch let error as FedTrackedCallError {
    guard let effect = error.effect else { throw error.underlying } // nothing was recorded
    saveDurably(effect.persistentString)   // before telling the user "couldn't confirm"
    // Later, in this launch or (with FedSQLiteStateStore) after a relaunch:
    let body = try await client.settledOutcome(of: effect, timeout: .seconds(60))
}
```

- `callManagementTracked` takes the same arguments as `callManagement` and
  behaves the same way (admission, retries, which `FedFailure` each outcome
  maps to). It returns `FedTrackedCallResult` (the body plus the effect id,
  which is nil for a successful read-only query) and throws
  `FedTrackedCallError`, which carries the unchanged error (`underlying`, or
  `failure` when it is a `FedFailure`) and the effect id. `effect` is nil for
  a read-only query and for a call refused before an effect id was assigned;
  such a call left nothing to recover.
- `settledOutcome(of:)` returns the outcome as the original call would have:
  the body for a result the module recorded, `FedFailure.moduleError` with the
  module's code, message and detail for a refusal the module recorded (only
  the code when the refusal body was over 64 KiB, so the message and detail
  may be missing), `FedFailure.mutationNotSent` when reconciliation proved the
  call never reached the module, and `FedFailure.indeterminateMutation` when
  reconciliation could only conclude that it may have arrived. If the mutation
  has not settled yet, it waits until reconciliation settles it, on a later
  session too. The wait honours task cancellation, and an optional `timeout`
  throws `FedSettledOutcomeError.timedOut`; neither decides anything.
- **A recovered result or refusal is the one the module recorded; "not sent"
  and "may have arrived" are what reconciliation concluded.** Do not call the
  module again for the same intent: sending the same mutation again is a
  second mutation. Only `mutationNotSent` means the call is safe to send
  again, as a new call.
- The outcome is read from the state store, never from the computer, so it can
  be read any number of times and works before `connect()`. It survives a
  relaunch only with `FedSQLiteStateStore` (a new store over the same
  directory); `FedMemoryStateStore` keeps it for the life of the process.
  Save the id as `effect.persistentString` (rebuild it with
  `FedEffectID(persistentString:)` or pass the text to
  `settledOutcome(ofPersistentString:)`) or as the `Codable` `FedEffectID`.
- The store keeps tracked outcomes only, for at most seven days after they
  settle, and a result body only up to 64 KiB. Whenever more than 256 rows are
  kept, expired rows are removed first and then the oldest settled ones, so an
  outcome can go sooner than seven days. Rows still waiting for their mutation
  to settle are never removed and can keep the total above 256 until they
  settle; there are few, since the phone sends a computer a new mutation only
  after the earlier ones to it have settled. Ordinary `callManagement` mutations keep nothing.
  Errors about the lookup itself are `FedSettledOutcomeError`, a separate enum
  rather than new `FedFailure` cases: `unknownEffect` (never tracked,
  malformed saved text, or removed by those limits), `resultNotRetained` (the
  result was over 64 KiB), `storeClosed` and `timedOut`.
- A custom `FedStateStore` gets default implementations of the two new
  requirements that keep nothing, so its outcomes always read as
  `unknownEffect`.

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

## Phone device approval (SubcFed)

`SubcFed` supplies the byte and cryptography layer for device approval, not
networking, notices or approval UI. It also builds the HTTP request bytes and
decodes success/refusal replies; the app owns HTTPS, auth and renewal.
Phone-approval signing domains are separate from enrollment dual proofs, renewal
and relay authentication.

- Decode response **bytes** with `ActionChallenge(json:)`,
  `RemovalChallenge(json:)`, `MergeChallenge(json:)`,
  `RegistrationChallenge(json:)` or `PossessionChallenge(json:)`. These are closed
  objects: unknown, missing and duplicate members refuse. Generations, versions
  and times must be canonical decimal **JSON strings**, never JSON numbers.
  Machine targets omit `x25519_pubkey_hex`; rendezvous targets require it and its
  value must equal the target device id. Predecessors are null or exactly the
  three-member retirement tuple. Scalar validators are also available through
  `DeviceApprovalGrammar`.
- Each decoder reconstructs the exact UTF-8 pipe string, without normalization
  or a trailing separator. Any returned `transcript` must match byte for byte.
  Actions/purposes select their own domains; there is no caller-selected action,
  tag or algorithm on the signing API. Registration exposes `deviceTranscript`
  and `enclaveTranscript`; the other challenges expose `transcript`.
- `challenge.target.fingerprint.comparisonCode` is computed locally from the
  target keys. `DeviceApprovalFingerprint(roster:ed25519Hex:x25519Hex:)` also
  computes it independently for a displayed card. It uses integer arithmetic
  and displays twelve decimal digits as `dddd-dddd-dddd`. Never display a
  server-supplied code. A challenge's claimed fingerprint must match the keys.

Ed25519 public keys undergo full compressed-point decoding and weak-point
rejection, including machine targets; CryptoKit's raw-key initializer alone is
not a point validator. Rendezvous X25519 targets must match their device id and
fingerprint. They reject exactly libsodium's seven `has_small_order` encodings
after clearing bit 255 for comparison (fourteen encodings including aliases).
No Diffie-Hellman probe or wider point-order test is applied to an X25519 target.

`DeviceApprovalRequests` owns the HTTP shapes, so the app need not construct or
parse JSON. `enrollmentStatus`, `approvals`, `removals` and `mergeStatus` return
GET requests with no body and closed, typed `decodeReply` results. List cursors
are opaque strings or null, never numbers; outbound cursor query values are
percent-encoded without changing their bytes. `retry_after_s` alone is a JSON
integer, exposed as `UInt64`.

`DeviceApprovalRequests.mergeList()` returns the bodiless GET for
`/v1/device/merges`, the held account merges involving the device's account, so
the app can show pending merges without having recorded their ids. Its
`decodeReply(_:statusCode:)` returns `DeviceApprovalMergeList` with `merges`
(each the same closed `DeviceApprovalMergeView` that `mergeStatus` returns) and
`cursor` (an opaque string or null; the service sends null until it defines
paging). The reply object and every merge are closed: unknown, missing and
duplicate members refuse. Ids are canonical ULIDs; generations, `state_version`,
`created_at_ms` and `execute_after_ms` are decimal strings at most 2^53 − 1, never
JSON numbers; `state` is `pending|cancelled|closing|merged` and `automatic_hold`
is null or one of the four defined holds, with any other value refused. Any
status other than 200, or a malformed body, throws `DeviceApprovalMergeListError`
and never yields an empty list. Its `refusal` holds the typed
`DeviceApprovalServerRefusal` when a non-200 body is one of the route's closed
refusals, and `reason` holds the `DeviceApprovalError` for a malformed body.
Computers read the same shape from `/v1/machine/merges`; the account service's
replies for both routes are pinned byte for byte in
`vectors/device-approval-merge-list-replies-v1`.

From those typed views, use `approvalChallenge`, `removalChallenge`,
`selfRevokeChallenge`, `mergeChallenge`, `registrationChallenge` or
`possessionChallenge` to obtain the challenge route's `method`, `path` and
`body`. Send those bytes through the app's account client. The returned request's
`decodeReply(_:statusCode:)` parses the whole route reply (these challenge routes
return standalone challenge objects, not wrappers), checks immutable context
against the requested card and returns a typed challenge or server refusal.

Then call `DeviceApprovalRequests.sign(_:credentials:reason:)` with a
`DeviceApprovalDecision` containing that challenge. It returns a
`DeviceApprovalDecisionRequest` with every signature already applied, a fixed
JSON member order and no extra whitespace. Actions and purposes still come from
the challenge, not a separate caller action. The separate `.selfRevoke` decision
requires an authorize/revoke challenge whose target and actor generations match.
Registration uses both the companion and enclave credentials. The app sends the
returned bytes unchanged, then calls the request's `decodeReply` with the raw
decision reply and HTTP status. Typed success variants contain the complete
closed enrollment/removal/merge view, registration, collection or refresh result.
Refusal decoding checks the status/code set for that exact route and returns
`DeviceApprovalServerRefusal`, including a numeric retry delay when applicable.

Collect uses `decodeCollectReply(_:trust:)` (or the overload with HTTP status),
with an explicit `DeviceApprovalCollectTrust` and **no default**:

- `.pinned(storedPin)` requires both the same account signing `key_id` and the
  same Ed25519 key bytes. A mismatch refuses with `account_key_mismatch`, the
  existing lockout condition; never silently replace the stored pin.
- `.firstEnrollment` accepts the delivered key for an account with no stored pin;
  the app must fetch the reply over authenticated HTTPS and choose this policy
  only when no pin is stored. SubcFed fully validates the
  delivered Ed25519 key, verifies the snapshot, and returns `accountSigningKey`
  for the app to store only after successful decoding. Verification failure
  returns no pin.

The HTTP snapshot's closed payload deliberately has no WebSocket `server_seq`.
SubcFed reuses `RdvSignedEnvelopeVerifier` on its raw payload before typed row
projection; it does not create another verifier. The generic decision decoder
refuses collect success without an explicit trust policy rather than guessing
whether a stored pin exists.

For example, after fetching a challenge and validating its context against the
card the person actually reviewed:

```swift
let challenge = try ActionChallenge(json: responseBytes)
// Compare account, enrollment/version, target keys, predecessor retirement effect,
// lifecycle and actor/key generation with the immutable card and installed identity.
let codeToDisplay = challenge.target.fingerprint.comparisonCode
let signatureHex = try DeviceApprovalActionSigner.sign(
    challenge, keyData: retainedActionKeyData, reason: "Confirm device access")
```

The app must own card/context comparison, eligibility, expiry/refresh, fixed
reason text, user confirmation and proof submission. Parsing does not establish
server authority or make an expired challenge current. Never sign from push
content, a device name or a transcript alone. The app must also check the
registration account/device/approval tuple against its installed identity and
retain the action-key id/public-key mapping for subsequent actions.

### Two separate keys, no storage side effects

`SecureEnclaveP256Signer` is the shared raw-byte primitive. `createKey()` returns
`SecureEnclaveP256Key` with an opaque `dataRepresentation` and
`x963PublicKeyHex`. `sign(_:keyData:reason:)` signs the exact bytes with SHA-256
once and returns `SecureEnclaveP256Signature.raw` (64-byte P1363) and `.der`
(ASN.1 DER), both derived from the same low-S-normalized signature. Each call
uses a fresh `LAContext`, zero authentication reuse and a nonempty fixed app
reason, and invalidates the context afterwards. It never stores a key and
refuses unsupported hardware or the simulator without a software fallback.

**Use one key per purpose.** Vault agent-secret approvals may use this primitive
with their own separate key and envelope bytes. Apps must never pass their
device-approval key directly to the raw-byte primitive: use
`DeviceApprovalActionSigner` so approval signs only checked, reconstructed
transcripts. The approval signer delegates internally to the shared primitive;
its public checked-challenge API is unchanged.

`DeviceApprovalActionSigner.createKey()` creates only a Secure Enclave P-256
key with `.privateKeyUsage` and `.userPresence`, accessible
`WhenUnlockedThisDeviceOnly`. It returns `DeviceApprovalActionKey` containing
`dataRepresentation` (an opaque enclave reference) and `p256PublicKeyHex` for
registration. The **app** must save `dataRepresentation` in its own device-only
Keychain **before registering**, and remove it on reset/unpair. On relaunch, load
the saved data with `try DeviceApprovalActionKey(dataRepresentation: savedData)`,
then repeat registration with the **same key**: request a fresh registration
challenge using `DeviceApprovalRequests.registrationChallenge(for: restoredKey)`
and submit both proofs again. Do not generate a replacement key just because the
previous registration's reply was lost. When the Secure Enclave is available,
a corrupt reference that cannot be restored throws `DeviceApprovalError` with
code `key_restoration_failed` and field `action_key`. The library never persists the reference. Unavailable Secure
Enclave hardware, including the simulator, refuses with
`secure_enclave_unavailable`; production never substitutes a software key.

The P-256 signer accepts only decoded A/D/M/R challenges, creates a new
`LAContext` for each signature with zero authentication reuse, and invalidates
it afterwards. Supply a fixed, localized app reason; do not interpolate device
names or server text. CryptoKit hashes transcript bytes once with SHA-256. Wire
signatures are 64-byte P1363 `r || s`, normalized to low-S and encoded as 128
lowercase hex digits, never DER. Registration verifies that the supplied key
reference matches the challenge's new P-256 public key before signing.

`DeviceApprovalCompanionSigner(keyStore:)` reads the existing retained Ed25519
key via `FedPrivateKeyStore.companionSigningPrivateKey()`, the same app-owned
source used for relay companion proofs. Its async `sign` overloads accept R
(the device line) and P (collect/refresh/withdraw for a rendezvous phone). They
sign raw transcript bytes without an extra hash. A missing companion key refuses;
no new key is created or stored. P also checks the key against the target's
Ed25519 public key. Register with **both** the companion R proof and the
enclave R proof; companion possession cannot approve a device or remove one.

`DeviceApprovalVerifier.verifyP256(signatureHex:transcript:publicKeyHex:)` and
`verifyEd25519` are public self-checks, throwing on invalid signatures. The
P-256 verifier rejects wrong-length/DER signatures, out-of-range scalars and
high-S twins instead of normalizing received signatures. All new refusals are
`DeviceApprovalError` with an extensible `Code` string struct and a `field`;
no cases were added to existing public enums.

### Software signing in tests only

Add the separate `SubcFedTestSupport` library product to your **test target only**.
Nothing enforces that: it is an ordinary public product, so keep it out of app
targets by convention and review. `SubcFed` itself has no public software-key path.
Use ordinary
`import SubcFed` and `import SubcFedTestSupport`; no `@testable`
import is required. `SoftwareDeviceApprovalSigner()` creates a software P-256
key on the simulator or host. `init(rawRepresentation:)` accepts a deterministic
test key. Its `actionKey` can be passed to
`DeviceApprovalRequests.registrationChallenge(for:)` to build registration
challenges, and
`try await signer.sign(decision, companionKeyStore: store)` builds signed decision
requests through the same request builder and low-S encoder as production.
The companion key store is required only for registration and possession proofs.
The action key's data is software-key material for tests, **not** an enclave
reference; do not pass it to the production restore/sign APIs. Never link
`SubcFedTestSupport` into an app product. Its cross-target hooks use Swift 5.9
`package` access, not public software-key overloads or SPI on `SubcFed`.

### Signed approval control notices and opt-in

Approvals are off by default. First open the identity-bound `FedSQLiteStateStore`
and set `FedRendezvousClient.Configuration.deviceApprovals` to a
`DeviceApprovalNoticeHandling(store:present:)`. The synchronous presentation sink
must emit the reviewed card/outcome before returning: the store then commits its
seen mark, and only then does the rendezvous client send a receipt-only ack.
A commit failure closes the connection without an ack. Reconnect replay is the
retry; there is no client ack outbox. `FedMemoryStateStore` implements the same
ordering for tests, but is not durable and must not be used for phone opt-in.

The app then sends `DeviceCapabilitiesUpdate.approvals(enabled:current:)` using
its `method`, `path`, and canonical `body`, authenticated with the device token.
Use the current self-row's capabilities; the builder preserves unknown valid
tokens, including `fed-serve`, while adding/removing only `device-approval-v1`.
The app owns this HTTP call. Constructing a client does not advertise the token.
On opt-out, send the removal update before disconnecting the approval reader.
If a notice arrives without configured handling, the client advances its cursor
but neither records nor acknowledges it, keeping discovery live and allowing
replay in a later configured session. `unhandledDeviceApprovalNoticeCount` counts
these deliveries; a diagnostic is logged once per session.

The client verifies the pinned account signature before decoding each notice;
`DeviceApprovalNotice.verify(_:pin:)` also supports fetched envelopes. Pending
views validate the full target keys and derive fingerprints and comparison codes
locally. The sink should remove the pending card when a resolved outcome arrives.
Seen marks retain only receipt metadata, not signed envelopes. Resolved outcomes
drop pending marks atomically; local pending deadlines and a seven-day maximum
bound retention. Pruning runs on connect and delivery; the app should also call
`pruneDeviceApprovalNotices(nowMS:)` on resume or periodic maintenance when no
connection is active. Expiry controls presentation only, never authorization.

Before signing an `ActionChallenge`, pass the **exact view the app showed** to
`DeviceApprovalActionSigner.sign(_:presentedView:keyData:reason:)`, or call
`requirePresentedView(_:)` before the original signing overload. A changed
predecessor refuses as `predecessor_mismatch` before user presence is requested.
The check also binds the account, operation, version and target keys.

`DeviceApprovalPush(json:)` parses only the fixed-alert APNs wake-up hint
and returns its kind and notice id. Fetch and verify the signed notice before
showing any action. Never approve from push bytes.

The rendezvous server's published notice set is pinned byte for byte in
`vectors/rdv-device-approval-notices-v1`; its checksum is checked by Swift tests.

Shared vectors live in `vectors/device-approval-rendezvous-v1` and
`vectors/device-approval-phone-v1` at the repository root. The first set is pinned
byte for byte; the second has a deterministic RFC 6979 generator. Swift tests
reconstruct all transcripts and verify the published signatures. Signing tests use
an internal software-key seam. A separate test calls production key creation only
to check refusal when the enclave is unavailable. Hardware user-presence behavior
still needs app integration tests on supported devices.

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
