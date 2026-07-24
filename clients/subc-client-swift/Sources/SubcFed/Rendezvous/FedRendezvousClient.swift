import Foundation
import CryptoKit
import OSLog

/// The identity material the control-WS client consumes from enrollment. The
/// device token is opaque — it is sent verbatim as the `Authorization: Bearer`
/// credential and never interpreted (docs/rdv-wire.md §3.3). The hello PoP
/// context fields (`account_id`, `token_id`, `token_version`) are supplied
/// alongside the token so the client can build the §2.3 hello context without
/// decoding the sealed token.
public struct FedRendezvousIdentity: @unchecked Sendable {
    public let accountId: String
    public let tokenId: String
    public let tokenVersion: String
    public let deviceToken: String
    public let x25519Key: FedNoiseKeyPair
    public let ed25519PrivateKey: Curve25519.Signing.PrivateKey

    public init(
        accountId: String,
        tokenId: String,
        tokenVersion: String,
        deviceToken: String,
        x25519Key: FedNoiseKeyPair,
        ed25519PrivateKey: Data
    ) throws {
        self.accountId = accountId
        self.tokenId = tokenId
        self.tokenVersion = tokenVersion
        self.deviceToken = deviceToken
        self.x25519Key = x25519Key
        self.ed25519PrivateKey = try Curve25519.Signing.PrivateKey(rawRepresentation: ed25519PrivateKey)
    }
}

/// Lifecycle states of the rendezvous control-WS client. The client is not
/// usable for discovery until it reaches `.ready` (the registry barrier has
/// landed). `.lockout` is the fatal §2.2 account_key_mismatch condition.
public enum FedRendezvousState: Sendable, Equatable {
    case disconnected
    case connecting
    case awaitingHelloChallenge
    case awaitingBarrier
    case ready
    case resyncing
    case lockout(RdvSignatureError)
    case failed(String)

    public static func == (lhs: FedRendezvousState, rhs: FedRendezvousState) -> Bool {
        switch (lhs, rhs) {
        case (.disconnected, .disconnected),
             (.connecting, .connecting),
             (.awaitingHelloChallenge, .awaitingHelloChallenge),
             (.awaitingBarrier, .awaitingBarrier),
             (.ready, .ready),
             (.resyncing, .resyncing):
            return true
        case (.lockout(let lhsError), .lockout(let rhsError)):
            return lhsError == rhsError
        case (.failed(let lhsMessage), .failed(let rhsMessage)):
            return lhsMessage == rhsMessage
        default:
            return false
        }
    }
}

public enum FedRendezvousError: Error, Equatable, Sendable {
    case closedBeforeHelloChallenge
    case closedBeforeBarrier
    case malformedMessage
    case barrierViolation
    case refused(RdvRefusal)
    /// `relay_open` (and other post-barrier signaling) requires the `.ready`
    /// state — the registry barrier must have landed before the client may open
    /// a relay pipe toward a peer it has discovered.
    case relayOpenRequiresReady
    /// A pending `awaitRelayGrant` waiter was released because the control
    /// session tore down (disconnect/resync) before a grant arrived. Grants are
    /// session-scoped signaling; the dial re-drives a fresh relay_open after
    /// reconnect rather than acting on a stale grant.
    case relayGrantSessionEnded
}

/// The rendezvous control-WS client (docs/rdv-wire.md §4). This is a CONNECTION
/// STATE MACHINE over one persistent WebSocket with push-based registry state —
/// NOT a fetch/publish RPC set. The server pushes every account device's
/// registry row; the client keeps a local MIRROR the server updates.
///
/// Lifecycle: connect (Bearer device token) → answer the `hello_challenge` with
/// the dual-PoP hello → apply the signed `registry_snapshot` barrier as truth →
/// consume signed deltas maintaining the per-recipient contiguous `server_seq`
/// cursor. A `server_seq` gap quarantines the stream and resyncs (reconnect for
/// a fresh barrier). A differing signed `key_id` is the fatal account_key_mismatch
/// lockout. `refresh()` tears down and reconnects (the app calls it on network
/// change); a fresh connect re-barriers with a new snapshot.
public actor FedRendezvousClient {
    public struct Configuration: @unchecked Sendable {
        /// Base control-WS URL, e.g. `wss://rdv.cortexkit.io/v1/ws`. Tests point
        /// this elsewhere; the transport factory receives it verbatim.
        public var controlURL: URL
        public var identity: FedRendezvousIdentity
        public var signingKeyPin: RdvAccountSigningKeyPin

        public init(controlURL: URL, identity: FedRendezvousIdentity, signingKeyPin: RdvAccountSigningKeyPin) {
            self.controlURL = controlURL
            self.identity = identity
            self.signingKeyPin = signingKeyPin
        }
    }

    private let configuration: Configuration
    private let streamFactory: @Sendable (URL) async throws -> any FedWebSocketStream
    private let verifier: RdvSignedEnvelopeVerifier

    private var stream: (any FedWebSocketStream)?
    private var readTask: Task<Void, Never>?

    private(set) public var state: FedRendezvousState = .disconnected
    private var mirror: [String: RdvRegistryRow] = [:]
    private var expectedNextSeq: UInt64?
    private var sessionSeq: UInt64 = 0
    private var quarantined = false
    private var lockoutError: RdvSignatureError?

    // Observable signal counts and surfaced state (read by tests and by the
    // notice/tombstone obligations the fed-module slices enforce).
    private(set) public var resyncCount = 0
    private(set) public var gapCount = 0
    private(set) public var droppedFrameCount = 0
    private(set) public var invalidSignatureCount = 0
    private(set) public var joinNotices: [RdvDeviceJoined] = []
    private(set) public var tombstones: [RdvTombstone] = []
    /// Verified `epoch_push` payloads received (membership revocations). This
    /// client has no org membership, so each is a logged no-op; the array makes
    /// receipt observable to tests and to the notice obligations above.
    private(set) public var epochPushes: [RdvEpochPush] = []
    private(set) public var lastRefusal: RdvRefusal?
    private(set) public var lastJoinReceipt: RdvDeviceJoinedReceipt?

    /// Loud, greppable line for an epoch_push that names an org this device is
    /// not a member of (every epoch push, for this non-member client). The fixed
    /// `RDV_EPOCH_PUSH_NOOP` token is what operators grep for.
    private let epochPushLog = Logger(subsystem: "io.cortexkit.subcfed", category: "rendezvous")

    // Relay signaling state (docs/rdv-wire.md §6.6). Grants are session-scoped:
    // buffered per peer until the dial ladder claims them, with waiters parked
    // for a peer whose grant has not arrived yet. The ledger enforces grant
    // single-use + redemption-TTL expiry at redemption time.
    private var grantLedger = FedRelayGrantLedger()
    private var bufferedGrants: [String: [RdvRelayGrant]] = [:]
    private var grantWaiters: [String: [CheckedContinuation<RdvRelayGrant, Error>]] = [:]
    /// The most recent relay_grant delivered (surfaced for tests and the app).
    private(set) public var lastRelayGrant: RdvRelayGrant?
    /// Count of relay_grant frames accepted off the control WS (both sides).
    private(set) public var relayGrantCount = 0

    /// Designated initializer with an injectable transport factory. Tests supply
    /// a scripted in-memory peer; production uses the URLSession convenience
    /// initializer below.
    public init(
        configuration: Configuration,
        streamFactory: @escaping @Sendable (URL) async throws -> any FedWebSocketStream
    ) {
        self.configuration = configuration
        self.streamFactory = streamFactory
        self.verifier = RdvSignedEnvelopeVerifier(pin: configuration.signingKeyPin)
    }

    /// Production initializer: upgrades with the native URLSessionWebSocketTask
    /// transport, authenticating the upgrade with the device token.
    public init(configuration: Configuration) {
        let deviceToken = configuration.identity.deviceToken
        self.configuration = configuration
        self.streamFactory = { url in
            try await FedURLSessionWebSocketStream.connect(url: url, bearerToken: deviceToken)
        }
        self.verifier = RdvSignedEnvelopeVerifier(pin: configuration.signingKeyPin)
    }

    // MARK: - Connection lifecycle

    /// Open the control WS, complete the hello dual-PoP, and apply the registry
    /// barrier. Returns once the client is `.ready`; a background read loop then
    /// consumes the server's pushed deltas for the life of the session.
    public func connect() async throws {
        try await establishSession()
        startReadLoop()
    }

    /// Refresh on network change: tear down the current control WS and reconnect.
    /// A fresh connect re-barriers with a new authoritative snapshot. The app
    /// calls this when the phone hops networks.
    public func refresh() async throws {
        await teardown()
        try await connect()
    }

    /// Tear down the session and stop consuming. The mirror is retained (R1:
    /// local state keeps working while discovery is down).
    public func disconnect() async {
        await teardown()
    }

    private func establishSession() async throws {
        state = .connecting
        let stream = try await streamFactory(configuration.controlURL)
        self.stream = stream

        state = .awaitingHelloChallenge
        let challenge = try await readHelloChallenge(from: stream)
        try await sendHello(answering: challenge, on: stream)

        state = .awaitingBarrier
        try await awaitBarrier(on: stream)
        state = .ready
    }

    private func teardown() async {
        let task = readTask
        readTask = nil
        task?.cancel()
        let currentStream = stream
        stream = nil
        if let currentStream {
            await currentStream.close()
        }
        // Wait for the old supervisor to finish so its cleanup (which closes the
        // stream and resets state) cannot race with a subsequent connect()'s new
        // session — refresh() reconnects immediately after teardown.
        _ = await task?.value
        state = .disconnected
        expectedNextSeq = nil
        quarantined = false
        sessionSeq = 0
        // Grants are session-scoped signaling (§4 rollover disposition: both the
        // opener's of_seq-bound grant and the target's unsolicited grant are
        // dropped with the session and re-driven by a fresh relay_open after
        // reconnect). Release any parked waiters and drop buffered grants so a
        // stale grant can never be acted on across a reconnect.
        let waiters = grantWaiters
        grantWaiters = [:]
        bufferedGrants = [:]
        for peerWaiters in waiters.values {
            for waiter in peerWaiters {
                waiter.resume(throwing: FedRendezvousError.relayGrantSessionEnded)
            }
        }
    }

    private func startReadLoop() {
        readTask?.cancel()
        readTask = Task { await self.readLoopSupervisor() }
    }

    // MARK: - Hello handshake

    private func readHelloChallenge(from stream: any FedWebSocketStream) async throws -> RdvHelloChallenge {
        while true {
            guard let message = try await stream.receive() else {
                throw FedRendezvousError.closedBeforeHelloChallenge
            }
            guard case .text(let text) = message else { continue }
            let object = try Self.parseObject(text)
            guard case .string(let type)? = object["type"] else { throw FedRendezvousError.malformedMessage }
            switch type {
            case "hello_challenge":
                return try RdvHelloChallenge.decode(object)
            case "refusal":
                throw FedRendezvousError.refused(try RdvRefusal.decode(object))
            default:
                // Ignore unexpected pre-hello frames (forward-compat).
                continue
            }
        }
    }

    private func sendHello(answering challenge: RdvHelloChallenge, on stream: any FedWebSocketStream) async throws {
        // The hello PoP context (docs/rdv-wire.md §2.3): all fields required, all
        // strings. The domain tag binds this proof to the control-WS hello surface.
        let context: [String: String] = [
            "domain": "rdv-v1/hello",
            "account_id": configuration.identity.accountId,
            "token_id": configuration.identity.tokenId,
            "token_version": configuration.identity.tokenVersion,
            "challenge_id": challenge.challengeId,
            "nonce": challenge.nonce,
            "server_eph_x25519_pubkey": challenge.serverEphX25519Pubkey,
            "x25519_pubkey_hex": configuration.identity.x25519Key.publicKey.lowercaseHex,
        ]
        let proof = try FedDualPoP(
            context: context,
            ed25519PrivateKey: configuration.identity.ed25519PrivateKey,
            x25519Key: configuration.identity.x25519Key,
            serverEphemeralX25519PublicKey: Data(hex: challenge.serverEphX25519Pubkey)
        )
        // hello consumes per-session seq "1".
        let hello = RdvHello(
            seq: "1",
            challengeId: challenge.challengeId,
            ed25519SigHex: proof.ed25519Signature.lowercaseHex,
            x25519ProofHex: proof.x25519Proof.lowercaseHex
        )
        try await stream.send(.text(try hello.encode()))
        sessionSeq = 1
    }

    /// Read post-hello frames until the signed registry_snapshot barrier lands and
    /// is applied as truth. The barrier is always the first server_seq frame the
    /// server sends after hello (§4 client cursor); anything signed before it is a
    /// protocol violation.
    private func awaitBarrier(on stream: any FedWebSocketStream) async throws {
        while true {
            guard let message = try await stream.receive() else {
                throw FedRendezvousError.closedBeforeBarrier
            }
            guard case .text(let text) = message else { continue }
            let object = try Self.parseObject(text)
            guard case .string(let type)? = object["type"] else { throw FedRendezvousError.malformedMessage }
            guard type == "signed" else { continue }
            let envelope = try RdvSignedEnvelope.decode(object)
            try verifier.verify(envelope) // key_id pin + signature; lockout throws
            let payload = try RdvSignedPayload.decode(envelope.payload)
            guard case .registrySnapshot(let snapshot) = payload else {
                throw FedRendezvousError.barrierViolation
            }
            applySnapshot(snapshot)
            return
        }
    }

    // MARK: - Post-barrier read loop

    private enum ReadLoopOutcome {
        case needsResync
        case lockout
        case closed
    }

    private func readLoopSupervisor() async {
        while !Task.isCancelled {
            let outcome = await runReadLoop()
            switch outcome {
            case .needsResync:
                resyncCount += 1
                if let stream { await stream.close() }
                stream = nil
                state = .resyncing
                do {
                    try await establishSession()
                } catch {
                    state = .failed("\(error)")
                    return
                }
            case .lockout:
                if let stream { await stream.close() }
                stream = nil
                if let lockoutError {
                    state = .lockout(lockoutError)
                } else {
                    state = .disconnected
                }
                return
            case .closed:
                if let stream { await stream.close() }
                stream = nil
                state = .disconnected
                return
            }
        }
    }

    private func runReadLoop() async -> ReadLoopOutcome {
        guard let stream else { return .closed }
        while true {
            let message: FedWebSocketMessage?
            do {
                message = try await stream.receive()
            } catch {
                return .closed
            }
            guard let message else { return .closed }
            // The control WS is text-only; binary frames belong to the relay pipe.
            guard case .text(let text) = message else { continue }
            if let outcome = processInbound(text) { return outcome }
        }
    }

    /// Process one inbound text frame. Returns a terminal outcome only when the
    /// stream must stop (resync, lockout); nil means keep reading.
    private func processInbound(_ text: String) -> ReadLoopOutcome? {
        guard let object = try? Self.parseObject(text) else { return nil }
        guard case .string(let type)? = object["type"] else { return nil }
        switch type {
        case "signed":
            return processSigned(object)
        case "relay_grant":
            return processRelayGrant(object)
        case "refusal":
            return processRefusal(object)
        default:
            return nil
        }
    }

    /// Handle a plain (unsigned) `relay_grant` control frame (§6.6). The grant
    /// carries a per-recipient `server_seq` and participates in the contiguous
    /// cursor exactly like a signed frame; because it is unsigned, a gap
    /// quarantines by sequence alone (§4 plain-frame rule). On an in-sequence
    /// grant the cursor advances and the grant is delivered to the dial ladder —
    /// BOTH the opener's `of_seq`-bound copy (side a) and the target's
    /// UNSOLICITED copy (side b), which the target must act on, never drop.
    private func processRelayGrant(_ object: RdvJSONObject) -> ReadLoopOutcome? {
        guard let grant = try? RdvRelayGrant.decode(object) else { return nil }
        if quarantined { return nil }
        switch classifyCursor(grant.serverSeq) {
        case .uninitialized, .dropped:
            return nil
        case .gap:
            return .needsResync
        case .apply:
            deliverRelayGrant(grant)
            advanceCursor()
            return nil
        }
    }

    /// Handle a plain (unsigned) `refusal` control frame (§8.1). A refusal is
    /// dispatched through the same per-recipient queue as signed payloads and
    /// `relay_grant`, so it consumes a `server_seq` from the contiguous space and
    /// must advance the cursor. Skipping it leaves the cursor behind and makes the
    /// NEXT frame read as a gap — and refusals cluster exactly when the network is
    /// already degraded (a dial ladder falling through rungs, rate-limited
    /// requests), so a burst would otherwise turn into a burst of full registry
    /// resyncs over a metered link.
    ///
    /// The cursor is processed BEFORE and INDEPENDENT of any interest in the
    /// refusal's contents: a kind this client ignores still consumed a sequence
    /// number. A replayed refusal after a reconnect classifies as `.dropped` and
    /// must not be surfaced again, or a pending relay_open would complete twice.
    private func processRefusal(_ object: RdvJSONObject) -> ReadLoopOutcome? {
        guard let refusal = try? RdvRefusal.decode(object) else { return nil }
        if quarantined { return nil }
        switch classifyCursor(refusal.serverSeq) {
        case .uninitialized, .dropped:
            return nil
        case .gap:
            return .needsResync
        case .apply:
            lastRefusal = refusal
            advanceCursor()
            return nil
        }
    }

    /// Classify a frame's `server_seq` against the per-recipient contiguous
    /// cursor (§4), recording the drop/gap side effects (counts, quarantine).
    /// `.apply` means act on the frame and then call `advanceCursor()`.
    private enum CursorDisposition {
        case apply, dropped, gap, uninitialized
    }

    private func classifyCursor(_ serverSeq: String) -> CursorDisposition {
        guard let expected = expectedNextSeq else { return .uninitialized }
        guard let seq = try? RdvDecimalString.parse(serverSeq) else { return .uninitialized }
        if seq < expected {
            droppedFrameCount += 1
            return .dropped
        }
        if seq > expected {
            quarantined = true
            gapCount += 1
            return .gap
        }
        return .apply
    }

    private func advanceCursor() {
        if let expected = expectedNextSeq {
            expectedNextSeq = expected + 1
        }
    }

    /// Deliver an in-sequence grant: resume a waiter parked for the grant's peer,
    /// or buffer it until the dial ladder claims it. Keyed by `peer` (the remote
    /// pubkey the grant connects to), which is the same lookup both the opener
    /// (side a) and the target (side b) use.
    private func deliverRelayGrant(_ grant: RdvRelayGrant) {
        relayGrantCount += 1
        lastRelayGrant = grant
        if var waiters = grantWaiters[grant.peer], !waiters.isEmpty {
            let waiter = waiters.removeFirst()
            grantWaiters[grant.peer] = waiters
            waiter.resume(returning: grant)
        } else {
            bufferedGrants[grant.peer, default: []].append(grant)
        }
    }

    private func processSigned(_ object: RdvJSONObject) -> ReadLoopOutcome? {
        guard let envelope = try? RdvSignedEnvelope.decode(object) else { return nil }

        // key_id pin FIRST: a differing key_id is the account_key_mismatch lockout
        // — stop consuming ALL cloud state for this account (§2.2).
        do {
            try RdvSignedEnvelopeVerifier.verifyKeyId(envelope.keyId, pinned: verifier.pin.keyId)
        } catch let error as RdvSignatureError {
            lockoutError = error
            return .lockout
        } catch {
            return .lockout
        }

        // Verify the signature over the canonical payload. An invalid signature is
        // dropped and counted — never acted on, never advances the cursor.
        do {
            try RdvSignedEnvelopeVerifier.verifySignature(
                payload: envelope.payload,
                signatureHex: envelope.signatureHex,
                publicKey: verifier.pin.ed25519PublicKey
            )
        } catch {
            invalidSignatureCount += 1
            return nil
        }

        guard let payload = try? RdvSignedPayload.decode(envelope.payload) else { return nil }

        // A VERIFIED registry_snapshot is always authoritative: it resets the
        // cursor, applies as truth, and ends any quarantine — even mid-gap, without
        // a reconnect (§4 verified-snapshot exception).
        if case .registrySnapshot(let snapshot) = payload {
            applySnapshot(snapshot)
            return nil
        }

        // Quarantined after a gap: act on nothing but a verified snapshot.
        if quarantined { return nil }

        // A payload that carries NO server_seq (epoch_push) is not part of the
        // per-recipient contiguous sequence: it contributes no cursor advance
        // and must not be gap-checked against the cursor. Apply it (a logged
        // no-op for this non-member client) and keep reading. Gap detection
        // below runs only over the seq-carrying kinds, so an epoch_push landing
        // between two seq-carrying payloads can never trip a resync.
        guard let seqString = payload.serverSeq else {
            return applyPayload(payload)
        }

        guard let expected = expectedNextSeq else { return nil }
        guard let seq = try? RdvDecimalString.parse(seqString) else { return nil }

        if seq < expected {
            // Regression or duplicate → drop + count, never act.
            droppedFrameCount += 1
            return nil
        }
        if seq > expected {
            // GAP → quarantine and resync (reconnect for a fresh barrier snapshot).
            quarantined = true
            gapCount += 1
            return .needsResync
        }

        // seq == expected: apply and advance the contiguous cursor.
        let outcome = applyPayload(payload)
        if outcome == nil {
            expectedNextSeq = expected + 1
        }
        return outcome
    }

    /// Apply a verified, in-sequence signed payload to local state. Returns
    /// `.needsResync` only for `resync_required`; nil otherwise.
    private func applyPayload(_ payload: RdvSignedPayload) -> ReadLoopOutcome? {
        switch payload {
        case .registrySnapshot(let snapshot):
            applySnapshot(snapshot)
            return nil
        case .registryDelta(let delta):
            applyDelta(delta)
            return nil
        case .deviceJoined(let notice):
            // NOTICE-ONLY: surface the un-dismissible join notice; never overwrite
            // registry truth from it (§5.5 rematerialized-notice rule).
            joinNotices.append(notice)
            return nil
        case .deviceJoinedReceipt(let receipt):
            lastJoinReceipt = receipt
            return nil
        case .tombstone(let tombstone):
            tombstones.append(tombstone)
            mirror[tombstone.x25519PubkeyHex] = nil
            return nil
        case .resyncRequired:
            quarantined = true
            return .needsResync
        case .epochPush(let push):
            // An epoch_push is a MEMBERSHIP REVOCATION for the receiving
            // device's own org — NOT a peer-registry change. It carried no
            // server_seq, so it contributed no cursor advance upstream. This
            // client (the phone) is a statically-paired Local(Verified) peer
            // with NO org membership, so every epoch push names an org it is
            // not a member of: a logged no-op. Receipt is NEVER grounds to
            // widen, reset, or refresh local trust.
            epochPushes.append(push)
            // SEAM — org-membership hard-stop (NOT implemented): if/when this
            // client gains org membership, THIS is the point that must hard-stop
            // its own sessions when `push.org` / `push.account` match its own
            // membership (the Mac daemon drains and fences live sessions here).
            // Until then there is no membership to revoke, so this stays a no-op.
            epochPushLog.notice("RDV_EPOCH_PUSH_NOOP org=\(push.org, privacy: .public) account=\(push.account, privacy: .public) new_epoch=\(push.newEpoch, privacy: .public) reason=\(push.reason.rawValue, privacy: .public): no membership in this org; logged no-op, no trust change")
            return nil
        }
    }

    // MARK: - Mirror mutation

    /// Apply the barrier snapshot as truth: it supersedes the entire prior mirror.
    private func applySnapshot(_ snapshot: RdvRegistrySnapshot) {
        var next: [String: RdvRegistryRow] = [:]
        for device in snapshot.devices {
            next[device.x25519PubkeyHex] = device
        }
        mirror = next
        if let seq = try? RdvDecimalString.parse(snapshot.serverSeq) {
            expectedNextSeq = seq + 1
        }
        quarantined = false
    }

    private func applyDelta(_ delta: RdvRegistryDelta) {
        switch delta.change {
        case .removed:
            mirror[delta.device.x25519PubkeyHex] = nil
        case .added, .updated, .online, .offline:
            mirror[delta.device.x25519PubkeyHex] = delta.device
        }
    }

    // MARK: - Candidate mirror query (read by the dial ladder built on this client)

    /// The current candidates for a peer by its X25519 pubkey hex, or nil if the
    /// peer is not in the mirror. This is the query the dial ladder (built on this
    /// client) uses to find a peer's reachability candidates.
    public func candidates(forPubkey pubkey: String) -> [RdvCandidate]? {
        mirror[pubkey]?.candidates
    }

    /// The full registry row for a peer by its X25519 pubkey hex.
    public func deviceRow(forPubkey pubkey: String) -> RdvRegistryRow? {
        mirror[pubkey]
    }

    /// A snapshot of the whole mirror (every account device's registry row).
    public func currentMirror() -> [RdvRegistryRow] {
        Array(mirror.values)
    }

    public var isReady: Bool {
        state == .ready
    }

    // MARK: - Relay signaling (relay_open / relay_grant)

    /// Send `relay_open` toward `peerPubkey` (docs/rdv-wire.md §6.6). ONLY the
    /// lower-key initiator of the pair calls this — the dial ladder makes that
    /// ownership decision; the higher-key peer never opens and instead redeems
    /// the unsolicited grant via `awaitRelayGrant(fromPeer:)`. Consumes the next
    /// per-session `seq`. `nonce` is 16 bytes of hex minted by the caller; a
    /// replayed open (same nonce) is refused `duplicate_rejected` server-side.
    public func relayOpen(to peerPubkey: String, nonce: String) async throws {
        guard state == .ready, let stream else { throw FedRendezvousError.relayOpenRequiresReady }
        sessionSeq += 1
        let open = RdvRelayOpen(seq: String(sessionSeq), to: peerPubkey, nonce: nonce)
        try await stream.send(.text(try open.encode()))
    }

    /// Await the next relay_grant whose `peer` is `peerPubkey` (the remote pubkey
    /// this device connects to). Returns a buffered grant immediately if one has
    /// already arrived; otherwise parks until the control WS delivers one. Both
    /// the opener (side a, `of_seq`-bound) and the target (side b, unsolicited)
    /// claim their grant through this one path. Throws
    /// `relayGrantSessionEnded` if the session tears down before a grant lands.
    public func awaitRelayGrant(fromPeer peerPubkey: String) async throws -> RdvRelayGrant {
        if var buffered = bufferedGrants[peerPubkey], !buffered.isEmpty {
            let grant = buffered.removeFirst()
            bufferedGrants[peerPubkey] = buffered
            return grant
        }
        return try await withCheckedThrowingContinuation { continuation in
            grantWaiters[peerPubkey, default: []].append(continuation)
        }
    }

    /// Validate and record redemption of `grant` at wall-clock `nowMs`
    /// (server-authoritative time, §1.2). Enforces grant single-use and the
    /// redemption-TTL expiry: a reused grant throws `alreadyRedeemed` and is
    /// never retried; an expired grant (zero remaining ms) throws `expired`.
    public func redeemRelayGrant(_ grant: RdvRelayGrant, nowMs: UInt64) throws {
        try grantLedger.redeem(grant: grant, nowMs: nowMs)
    }

    /// Whether a pipe has already been redeemed on this device (single-use).
    public func isGrantRedeemed(pipeID: String) -> Bool {
        grantLedger.isRedeemed(pipeID: pipeID)
    }

    /// True once the client has entered the fatal §2.2 account_key_mismatch
    /// lockout (a signed envelope arrived with a key_id differing from the pin).
    public var isLockedOut: Bool {
        if case .lockout = state { return true }
        return false
    }

    // MARK: - Helpers

    private static func parseObject(_ text: String) throws -> RdvJSONObject {
        try RdvJSONValue.parseObject(Data(text.utf8))
    }
}
