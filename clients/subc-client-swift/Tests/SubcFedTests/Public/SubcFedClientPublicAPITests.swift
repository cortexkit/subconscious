import CryptoKit
import Foundation
import Network
import XCTest
@testable import SubcFed

final class SubcFedClientPublicAPITests: XCTestCase {
    func testPublicFailureVocabularyCoversRequiredClasses() {
        let failures: [FedFailure] = [
            .notDialOwner,
            .unsupportedEnrollmentClass,
            .invalidProfile(field: "default_deadline_ms"),
            .candidateRejected(reason: .unverifiedPeerLAN),
            .candidateTimedOut(stage: .noiseHandshake),
            .relayAuthenticationFailed(code: "bad_token"),
            .responderKeyMismatch,
            .accountKeyMismatch,
            .noiseAuthenticationFailed,
            .framingViolation,
            .protocolViolation(byeCode: "fed_bad_frame"),
            .catalogTargetUnavailable,
            .fedBodyTooLarge,
            .fedEffectsUnsupported,
            .storeCorrupt,
            .storeUnavailable,
            .storeMigrationFailed,
            .reservationFailed,
            .persistenceFailed,
            .cancelled,
            .suspended,
            .disconnected,
            .indeterminateMutation,
            .admissionQueueFull,
            .admissionQueueTimedOut,
            .noEligibleCandidates([]),
            .allCandidatesFailed([]),
        ]
        XCTAssertEqual(failures.count, 27)
        XCTAssertEqual(Self.candidateStages.count, 5)
        XCTAssertEqual(Self.rejectionReasons.count, 6)
    }

    func testNotDialOwnerUsesKeyStorePublicKeyBeforeAttemptOrCarrier() async throws {
        let dialCounter = DialCounter()
        let factory = RecordingDialFactory { _, _ in
            await dialCounter.increment()
            throw FedFailure.disconnected
        }
        // Local private 0xF0 yields a public key that sorts above responder 0x10
        // when both sides publish addresses, so the local peer is not dial owner.
        let localPrivate = Data(repeating: 0xF0, count: 32)
        let responderPrivate = Data(repeating: 0x10, count: 32)
        let keyStore = try FedMemoryPrivateKeyStore(noisePrivateKey: localPrivate)
        let localPublic = try await keyStore.staticPublicKey()
        let responderPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: responderPrivate)
        XCTAssertFalse(
            FedDialOwnership.isLocalDialOwner(
                localPublicKey: localPublic,
                responderPublicKey: responderPublic,
                facts: FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: true)
            )
        )

        let profile = try FedPeerProfile(
            peerIdentity: "peer",
            responderStaticPublicKey: responderPublic,
            enrollmentClass: .human,
            isVerified: true,
            dialOwnership: FedDialOwnershipFacts(
                localPublishesAddress: true,
                remotePublishesAddress: true
            ),
            candidates: [
                .lanDirect(try FedLANDirectCandidate(
                    candidateID: "lan-1",
                    host: "192.168.1.10",
                    port: 7700
                )),
            ]
        )
        let client = SubcFedClient(
            profile: profile,
            keyStore: keyStore,
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )

        do {
            try await client.connect()
            XCTFail("expected notDialOwner")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .notDialOwner)
        }

        let state = await client.state
        let attempt = await client.lastAttemptID
        let carriers = await client.carrierOperationsStarted
        let dials = await dialCounter.value
        XCTAssertEqual(state, .disconnected(reason: .notDialOwner))
        XCTAssertNil(attempt)
        XCTAssertEqual(carriers, 0)
        XCTAssertEqual(dials, 0)
    }

    func testUnsupportedEnrollmentClassRefusesBeforeCarrier() async throws {
        let profile = try FedPublicTestSupport.humanProfile(enrollment: .unsupported("service"))
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: RecordingDialFactory()
        )
        do {
            try await client.connect()
            XCTFail("expected unsupportedEnrollmentClass")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .unsupportedEnrollmentClass)
        }
        let attempt = await client.lastAttemptID
        let carriers = await client.carrierOperationsStarted
        XCTAssertNil(attempt)
        XCTAssertEqual(carriers, 0)
    }

    func testUnverifiedLANRejectedWithoutCarrierAndAttemptID() async throws {
        let profile = try FedPublicTestSupport.humanProfile(isVerified: false)
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: RecordingDialFactory()
        )
        do {
            try await client.connect()
            XCTFail("expected noEligibleCandidates")
        } catch let failure as FedFailure {
            guard case .noEligibleCandidates(let retained) = failure else {
                return XCTFail("unexpected \(failure)")
            }
            XCTAssertEqual(retained.first?.reason, .rejected(.unverifiedPeerLAN))
        }
        let attempt = await client.lastAttemptID
        let carriers = await client.carrierOperationsStarted
        XCTAssertNil(attempt)
        XCTAssertEqual(carriers, 0)
    }

    func testAttemptIdentifierIs32LowercaseHexWhenDialBegins() async throws {
        let attemptBox = AttemptBox()
        let factory = RecordingDialFactory { _, context in
            await attemptBox.store(context.attemptID)
            throw FedFailure.disconnected
        }
        let profile = try FedPublicTestSupport.humanProfile()
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            entropy: FedFixedNoiseEntropy(Data(repeating: 0xAB, count: 16)),
            dialFactory: factory
        )

        do {
            try await client.connect()
            XCTFail("expected dial failure after attempt mint")
        } catch {
            // Post-attempt failure is expected; attempt ID is the assertion.
        }

        let attempt = await client.lastAttemptID
        let recorded = await attemptBox.value
        let carriers = await client.carrierOperationsStarted
        XCTAssertEqual(attempt?.count, 32)
        XCTAssertEqual(attempt, String(repeating: "ab", count: 16))
        XCTAssertTrue(attempt?.unicodeScalars.allSatisfy {
            (0x30...0x39).contains($0.value) || (0x61...0x66).contains($0.value)
        } ?? false)
        XCTAssertEqual(recorded, attempt)
        XCTAssertEqual(carriers, 1)
    }

    func testOwnershipUsesProfilePinnedResponderKey() async throws {
        let firstPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0x01, count: 32))
        let secondPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0xFE, count: 32))
        // Curve25519 public keys are not ordered like their private seeds; pick the
        // actual lexicographic lower/higher pair after derivation.
        let lower = firstPublic.fedLexicographicallyPrecedes(secondPublic) ? firstPublic : secondPublic
        let higher = lower == firstPublic ? secondPublic : firstPublic
        XCTAssertTrue(lower.fedLexicographicallyPrecedes(higher))

        XCTAssertTrue(
            FedDialOwnership.isLocalDialOwner(
                localPublicKey: lower,
                responderPublicKey: higher,
                facts: FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: true)
            )
        )
        XCTAssertFalse(
            FedDialOwnership.isLocalDialOwner(
                localPublicKey: higher,
                responderPublicKey: lower,
                facts: FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: true)
            )
        )
        // Origin-only phone: remote publishes, local does not → always dial owner
        // regardless of key order.
        XCTAssertTrue(
            FedDialOwnership.isLocalDialOwner(
                localPublicKey: higher,
                responderPublicKey: lower,
                facts: .localOriginOnly
            )
        )
    }

    func testDisconnectReturnsIdleAndBlocksImplicitRedial() async throws {
        let profile = try FedPublicTestSupport.humanProfile()
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: RecordingDialFactory()
        )
        await client.disconnect()
        let idle = await client.state
        XCTAssertEqual(idle, .idle)

        let updated = try FedPublicTestSupport.humanProfile(
            candidates: [
                .lanDirect(try FedLANDirectCandidate(
                    candidateID: "lan-2",
                    host: "192.168.1.20",
                    port: 7700
                )),
            ]
        )
        try await client.updateProfile(updated)
        let stillIdle = await client.state
        let carriers = await client.carrierOperationsStarted
        XCTAssertEqual(stillIdle, .idle)
        XCTAssertEqual(carriers, 0)
    }

    func testSuspendAndResumeLifecycle() async throws {
        let profile = try FedPublicTestSupport.humanProfile()
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: RecordingDialFactory()
        )
        await client.suspend()
        let dormant = await client.state
        XCTAssertEqual(dormant, .dormant)

        do {
            try await client.resume()
        } catch {
            // Expected: no real carrier.
        }
        let attempt = await client.lastAttemptID
        XCTAssertEqual(attempt?.count, 32)

        await client.disconnect()
        let idle = await client.state
        XCTAssertEqual(idle, .idle)
        try await client.resume()
        let stillIdle = await client.state
        XCTAssertEqual(stillIdle, .idle)
    }

    func testJSONAndManagementTargetSurface() throws {
        let object = try FedJSONObject(validating: [
            "board": .string("main"),
            "limit": .integer(10),
        ])
        XCTAssertEqual(object["limit"], .integer(10))
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        XCTAssertEqual(target.moduleID, "alfonso-core")
        XCTAssertThrowsError(try FedManagementTarget(moduleID: "  "))
    }

    func testManagementTargetDecodeValidatesModuleID() throws {
        let decoder = JSONDecoder()
        // A synthesized decode would accept a blank moduleID and bypass the
        // validating init; the custom decode must reject it.
        let blank = Data("{\"moduleID\":\"   \"}".utf8)
        XCTAssertThrowsError(try decoder.decode(FedManagementTarget.self, from: blank))
        let missing = Data("{}".utf8)
        XCTAssertThrowsError(try decoder.decode(FedManagementTarget.self, from: missing))

        // A valid payload decodes, trims, and round-trips through encode/decode.
        let valid = Data("{\"moduleID\":\"  alfonso-core  \"}".utf8)
        let decoded = try decoder.decode(FedManagementTarget.self, from: valid)
        XCTAssertEqual(decoded.moduleID, "alfonso-core")
        let encoded = try JSONEncoder().encode(decoded)
        let roundTripped = try decoder.decode(FedManagementTarget.self, from: encoded)
        XCTAssertEqual(roundTripped, decoded)
    }

    func testAdmissionPolicyReturnsStoredValidatedSnapshotWithoutTrapping() throws {
        let profile = try FedPublicTestSupport.humanProfile()
        // Accessing admissionPolicy must return the validated snapshot built at
        // construction (no re-validation, no force-try trap).
        let policy = profile.admissionPolicy
        XCTAssertEqual(policy.queueCapacity, profile.queueCapacity)
        XCTAssertEqual(policy.queueWaitTimeoutMs, profile.queueWaitTimeoutMs)
        XCTAssertEqual(policy.defaultDeadlineMs, profile.defaultDeadlineMs)

        // Custom policy ranges survive construction and are reflected exactly.
        let custom = try FedPeerProfile(
            peerIdentity: "peer",
            responderStaticPublicKey: try FedPublicTestSupport.responderPublicKey(),
            enrollmentClass: .human,
            isVerified: true,
            candidates: [
                .lanDirect(try FedLANDirectCandidate(
                    candidateID: "lan-1", host: "192.168.1.10", port: 7700
                )),
            ],
            defaultDeadlineMs: 5_000,
            queueCapacity: 8,
            queueWaitTimeoutMs: 250
        )
        XCTAssertEqual(custom.admissionPolicy.defaultDeadlineMs, 5_000)
        XCTAssertEqual(custom.admissionPolicy.queueCapacity, 8)
        XCTAssertEqual(custom.admissionPolicy.queueWaitTimeoutMs, 250)
    }

    func testDialPolicyAndStateStoreProtocolsArePublic() async throws {
        let policy = FedDialPolicy()
        XCTAssertEqual(policy.noiseHandshake, .seconds(10))
        let store: any FedStateStore = FedMemoryStateStore()
        let publicKey = try FedPublicTestSupport.localPublicKey()
        _ = try await store.open(localPublicKey: publicKey)
        let snapshot = try await store.snapshot()
        XCTAssertFalse(snapshot.global.localIncarnation.isEmpty)
    }

    func testLANHygieneBoundaries() throws {
        let snapshot = try FedPublicTestSupport.observedHomeLAN()
        XCTAssertNil(
            FedLANCandidateHygiene.classify(
                address: IPv4Address("192.168.1.1")!,
                peerVerified: true,
                snapshot: snapshot
            )
        )
        XCTAssertEqual(
            FedLANCandidateHygiene.classify(
                address: IPv4Address("192.168.2.1")!,
                peerVerified: true,
                snapshot: snapshot
            ),
            .outsideObservedPrivateSubnet
        )
        XCTAssertEqual(
            FedLANCandidateHygiene.classify(
                address: IPv4Address("8.8.8.8")!,
                peerVerified: true,
                snapshot: snapshot
            ),
            .addressClassNotAllowed
        )
        XCTAssertEqual(
            FedLANCandidateHygiene.classify(
                address: IPv4Address("127.0.0.1")!,
                peerVerified: true,
                snapshot: snapshot
            ),
            .addressClassNotAllowed
        )
        XCTAssertEqual(
            FedLANCandidateHygiene.classify(
                address: IPv4Address("192.168.1.1")!,
                peerVerified: false,
                snapshot: snapshot
            ),
            .unverifiedPeerLAN
        )
    }

    /// The connection-state vocabulary is a closed public contract: a consumer
    /// switching over it exhaustively must be forced to consider any new case
    /// rather than silently falling into a default.
    ///
    /// The assertion is the EXHAUSTIVE SWITCH, not the count. Counting a
    /// hand-written array proves only that the array has as many entries as
    /// were typed into it — adding a ninth case to the enum leaves such a test
    /// green, which is exactly the drift it appears to guard. The switch below
    /// fails to COMPILE when a case is added or removed, so the guard cannot be
    /// satisfied by an out-of-date fixture.
    func testConnectionStateCasesAreClosed() {
        let states: [FedConnectionState] = [
            .idle,
            .dialing(attemptID: String(repeating: "ab", count: 16), candidateID: "c", stage: .carrierConnect),
            .authenticating(attemptID: String(repeating: "ab", count: 16), candidateID: "c", kind: .noise),
            .negotiating(attemptID: String(repeating: "ab", count: 16), candidateID: "c"),
            .ready(sessionID: "s"),
            .reconnectWaiting(deadlineNanoseconds: 1, lastFailure: .disconnected),
            .dormant,
            .disconnected(reason: .cancelled),
        ]

        // Every case is named here. Adding one to FedConnectionState without
        // adding it to this switch is a compile error, which is the point.
        var seen = Set<String>()
        for state in states {
            switch state {
            case .idle: seen.insert("idle")
            case .dialing: seen.insert("dialing")
            case .authenticating: seen.insert("authenticating")
            case .negotiating: seen.insert("negotiating")
            case .ready: seen.insert("ready")
            case .reconnectWaiting: seen.insert("reconnectWaiting")
            case .dormant: seen.insert("dormant")
            case .disconnected: seen.insert("disconnected")
            }
        }

        // And the sample above covers every case rather than repeating one:
        // without this, the switch could be exhaustive while the array exercised
        // a single state eight times.
        XCTAssertEqual(seen.count, states.count, "each sampled state must be a distinct case")
    }

    private static let candidateStages: [FedCandidateStage] = [
        .carrierConnect, .webSocketUpgrade, .relayAuthentication, .noiseHandshake, .fedNegotiation,
    ]

    private static let rejectionReasons: [CandidateRejectionReason] = [
        .unverifiedPeerLAN,
        .missingObservedPrivateSubnet,
        .invalidAddress,
        .addressClassNotAllowed,
        .outsideObservedPrivateSubnet,
        .unsupportedCandidateClass,
    ]
}

// MARK: - Helpers

private actor AttemptBox {
    private(set) var value: String?
    func store(_ value: String) { self.value = value }
}

private actor DialCounter {
    private(set) var value = 0
    func increment() { value += 1 }
}
