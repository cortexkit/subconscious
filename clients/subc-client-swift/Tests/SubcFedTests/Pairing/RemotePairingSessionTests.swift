import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class RemotePairingSessionTests: XCTestCase {
    func testDefiniteFramingFailuresDoNotLeaveAStatusReceipt() async throws {
        for failure in ["oversized", "empty", "truncated outer", "truncated inner"] {
            let fixture = try await RemotePairingFixture()
            let pipe = RemotePairingPipe()
            let client = try fixture.client(pipes: [pipe])
            let target = Task {
                let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
                _ = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe)
                _ = try await wire.receive()
                switch failure {
                case "oversized": try await pipe.target.write(Data([0, 0, 1, 0]))
                case "empty": try await pipe.target.write(Data(repeating: 0, count: 4))
                case "truncated outer": try await pipe.target.write(Data([48, 0, 0, 0, 0]))
                default: try await wire.session.sendTransportPayload(Data([55, 0, 0, 0, 123]))
                }
                await wire.close()
            }
            let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
            do {
                let outcome = try await pending.confirmMatch(true)
                XCTFail("\(failure) was incorrectly recoverable as \(outcome)")
            } catch { XCTAssertEqual(error as? FedRemotePairingError, .protocolViolation, failure) }
            XCTAssertNil(try fixture.store.load(scope: fixture.scope), failure)
            try await target.value
            await fixture.rendezvous.disconnect()
        }
    }

    func testUnsolicitedPairingGrantAtIngressNeverReachesOrdinaryDial() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true, advertiseDirect: false)
        let peer = fixture.targetKey.publicKey.lowercaseHex
        let pipe = RemotePairingPipe()
        let ordinary = Task {
            let grant = try await fixture.rendezvous.awaitRelayGrant(fromPeer: peer)
            if grant.side == .b {
                // A normal dial treats itself as Noise initiator. If the router
                // misdelivers this ticket, it would write message 1 to T.
                let noise = try FedNoiseIKInitiator(staticKey: fixture.deviceKey, pinnedResponderStatic: fixture.targetKey.publicKey)
                try await pipe.device.sendNoiseMessage(noise.writeMessage1())
            }
            return grant
        }
        let refused = String(repeating: "1", count: 26)
        try await pushGrant(fixture, serverSeq: "2", ofSeq: nil, side: .b, pipeID: refused)
        // Use a side-a sentinel to distinguish an ignored ticket from a waiter
        // that happened not to run yet. Nothing in this test starts pairing.
        let sentinel = String(repeating: "2", count: 26)
        try await pushGrant(fixture, serverSeq: "3", ofSeq: "2", side: .a, pipeID: sentinel)
        let delivered = try await fedTestWithTimeout { try await ordinary.value }
        XCTAssertEqual(delivered.pipeID, sentinel)
        let writes = await pipe.writes
        XCTAssertEqual(writes[0], 0, "D must not send Noise bytes for a refused side-b grant")
        let redeemed = await fixture.rendezvous.isGrantRedeemed(pipeID: refused)
        XCTAssertFalse(redeemed)
        await fixture.rendezvous.disconnect()
    }

    func testUnsolicitedGrantRoutesToThePendingCeremonyAndChecksItsNoisePurpose() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true, advertiseDirect: false)
        let mismatched = LoopbackWebSocketPair(), pairing = LoopbackWebSocketPair()
        let wrongPipe = String(repeating: "3", count: 26), pairPipe = String(repeating: "4", count: 26)
        let client = try fixture.client(pipes: [], upgrade: { candidate in
            switch candidate.pipeID {
            case wrongPipe: return mismatched.client
            case pairPipe: return pairing.client
            default: XCTFail("unknown relay grant was redeemed"); throw FedRemotePairingError.unavailable
            }
        })
        let start = Task { try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous) }
        func waitForPairingGrantWaiter() async throws {
            try await fedTestWithTimeout {
                while await fixture.rendezvous.pendingPairRelayWaiterCount != 1 { await Task.yield() }
            }
        }
        try await waitForPairingGrantWaiter()
        try await pushGrant(fixture, serverSeq: "2", ofSeq: nil, side: .b, pipeID: wrongPipe)
        try await readyRelay(mismatched.server)
        let wrongPurpose = try FedNoiseIKInitiator(staticKey: fixture.targetKey, pinnedResponderStatic: fixture.deviceKey.publicKey)
        try await mismatched.server.send(.binary(FedOuterRecordCodec.encode(wrongPurpose.writeMessage1())))
        let response = try await fedTestWithTimeout { try await mismatched.server.receive() }
        XCTAssertNil(response, "an ordinary message 1 on a pairing ticket must get no Noise reply")

        try await waitForPairingGrantWaiter()
        try await pushGrant(fixture, serverSeq: "3", ofSeq: nil, side: .b, pipeID: pairPipe)
        try await readyRelay(pairing.server)
        let target = Task {
            let (wire, hash) = try await fixture.targetWire(FedWebSocketRecordCarrier(stream: pairing.server), mode: .targetRelay)
            let (_, code) = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .targetRelay)
            let match = try await wire.receive()
            XCTAssertEqual(match.typeName, FedRemotePairCodec.match)
            XCTAssertEqual(match.header["match"], .boolean(false))
            return code
        }
        let pending = try await fedTestWithTimeout { try await start.value }
        let outcome = try await pending.confirmMatch(false)
        XCTAssertEqual(outcome, .notGranted)
        let peerCode = try await target.value
        XCTAssertEqual(pending.code, peerCode)
        await fixture.rendezvous.disconnect()
    }

    private func readyRelay(_ server: LoopbackWebSocketStream) async throws {
        let ephemeral = Curve25519.KeyAgreement.PrivateKey().publicKey.rawRepresentation.lowercaseHex
        try await server.send(.text("""
            {"type":"relay_challenge","challenge_id":"fixture-challenge","nonce":"fixture-nonce","server_eph_x25519_pubkey":"\(ephemeral)","expires_at_ms":"4102444800000"}
            """))
        let hello = try await fedTestWithTimeout { try await server.receive() }
        guard case .text = hello else { throw FedCarrierError.carrierClosed }
        try await server.send(.text(#"{"type":"relay_ready"}"#))
    }

    func testBufferedSecondNoiseRecordRefusesCeremonyAndStatusGrant() async throws {
        for status in [false, true] {
            let fixture = try await RemotePairingFixture()
            let pipe = RemotePairingPipe()
            let client = try fixture.client(pipes: [pipe])
            if status {
                let window = RemotePairingFixture.window(.phone)
                let receipt = FedRemotePairReceipt(windowID: window.id, generation: window.generation,
                    transcriptHash: Data(repeating: 0xC4, count: 32), targetKey: fixture.targetKey.publicKey, matched: true)
                try fixture.store.replace(scope: fixture.scope, record: JSONEncoder().encode(receipt))
            }
            let target = Task {
                let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target), status: status)
                if !status { _ = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe) }
                _ = try await wire.receive()
                await pipe.coalesceNextTwoTargetWrites()
                try await wire.send(fixture.result("granted"))
                try await wire.send(fixture.result("granted"))
            }
            do {
                let outcome: FedRemotePairingOutcome
                if status { outcome = try await client.remotePairingStatus(rendezvous: fixture.rendezvous) }
                else {
                    let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
                    outcome = try await pending.confirmMatch(true)
                }
                XCTFail("\(status ? "status" : "ceremony") accepted buffered duplicate: \(outcome)")
            } catch { XCTAssertEqual(error as? FedRemotePairingError, .protocolViolation) }
            try await target.value
            await fixture.rendezvous.disconnect()
        }
    }

    func testImmediateRefusalAfterWindowWorksInOneOrTwoNoiseRecords() async throws {
        for (sameRecord, bye) in [(true, true), (true, false), (false, true), (false, false)] {
            let fixture = try await RemotePairingFixture()
            let pipe = RemotePairingPipe()
            let client = try fixture.client(pipes: [pipe])
            let target = Task {
                let (wire, _) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
                let binding = RemotePairingFixture.binding()
                let terminal = bye ? FedFrame(type: "bye", fields: ["code": .string("fed_pairing_expired")])
                    : fixture.result("not_granted")
                if sameRecord {
                    try await wire.session.sendTransportPayload(FedRemotePairCodec.encode(binding) + FedRemotePairCodec.encode(terminal))
                } else {
                    await pipe.coalesceNextTwoTargetWrites()
                    try await wire.send(binding)
                    try await wire.send(terminal)
                }
            }
            do {
                _ = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
                XCTFail("a terminal refusal must not show a pairing code")
            } catch {
                XCTAssertEqual(error as? FedRemotePairingError, .refused(code: bye ? "fed_pairing_expired" : "fed_pairing_declined"))
            }
            try await target.value
            let writes = await pipe.writes
            XCTAssertEqual(writes[0], 1, "D sent a commit after T had already refused")
            await fixture.rendezvous.disconnect()
        }
    }

    func testGrantReadDuringMatchWriteRefusesEvenIfTheUIAlreadyAnswered() async throws {
        let fixture = try await RemotePairingFixture()
        let pipe = RemotePairingPipe()
        // In direct IK these are message 1, commit, reveal, then local match.
        await pipe.suspendDeviceWrite(number: 4)
        let client = try fixture.client(pipes: [pipe])
        let target = Task {
            let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
            _ = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe)
            try await fedTestWithTimeout {
                while !(await pipe.deviceWriteIsSuspended) { await Task.yield() }
            }
            XCTAssertNotNil(try fixture.store.load(scope: fixture.scope))
            // The transport still owns the pending write. D receives the result
            // first, and closing this link will make that write report failure.
            try await wire.send(fixture.result("granted"))
        }
        let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
        let answer = Task { try await pending.confirmMatch(true) }
        do {
            try await target.value
            try await fedTestWithTimeout { while !(await pipe.closed) { await Task.yield() } }
            let outcome = try await answer.value
            XCTFail("early result with a failed match write settled as \(outcome)")
        } catch {
            XCTAssertEqual(error as? FedRemotePairingError, .protocolViolation)
        }
        await pipe.close()
        XCTAssertNil(try fixture.store.load(scope: fixture.scope))
        await fixture.rendezvous.disconnect()
    }

    func testPinRequiresOwnMatchAndTheAuthenticatedTargetKey() async throws {
        let suppliedMachineID = "0123456789abcdef0123456789abcdef"
        let scenarios: [(early: Bool, wrongKey: Bool, machineID: String?)] = [
            (false, false, nil), (false, false, suppliedMachineID), (true, false, suppliedMachineID),
            (false, true, suppliedMachineID),
        ]
        for (early, wrongKey, machineID) in scenarios {
            let fixture = try await RemotePairingFixture()
            let pipe = RemotePairingPipe()
            let client = try fixture.client(pipes: [pipe])
            let target = Task {
                let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
                let (_, code) = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe)
                if !early {
                    let match = try await wire.receive()
                    XCTAssertEqual(match.typeName, FedRemotePairCodec.match)
                    XCTAssertEqual(match.header["match"], .boolean(true))
                }
                try await wire.send(fixture.result(wrongKey ? "granted_wrong_key" : "granted", machineID: machineID))
                return code
            }
            let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
            if early {
                try await fedTestWithTimeout {
                    while !(await pipe.closed) { await Task.yield() }
                }
            }
            do {
                let outcome = try await pending.confirmMatch(true)
                XCTAssertFalse(early || wrongKey, "an early or wrong-key grant must never authorize a pin")
                if case .granted(let grant) = outcome {
                    XCTAssertEqual(grant.computerStaticPublicKey, fixture.targetKey.publicKey)
                    XCTAssertEqual(grant.computerMachineID, machineID)
                    XCTAssertEqual(grant.rawName, "Selected Computer")
                    XCTAssertEqual(grant.displayName, "Selected Computer")
                } else { XCTFail("expected a grant, got \(outcome)") }
            } catch {
                XCTAssertEqual(error as? FedRemotePairingError, early ? .protocolViolation : (wrongKey ? .wrongTargetKey : nil))
            }
            let code = try await target.value
            XCTAssertEqual(pending.code, code)
            XCTAssertEqual(pending.targetName, "Selected Computer")
            XCTAssertEqual(pending.confirmerName, "Confirming é")
            XCTAssertNil(try fixture.store.load(scope: fixture.scope))
            await fixture.rendezvous.disconnect()
        }
    }

    func testLostResultKeepsOnlyReceiptAndFreshClientChecksStatus() async throws {
        let fixture = try await RemotePairingFixture()
        let ceremony = RemotePairingPipe(), status = RemotePairingPipe()
        let startingClient = try fixture.client(pipes: [ceremony])
        let target = Task {
            let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: ceremony.target))
            let (transcript, _) = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: ceremony)
            let answer = try await wire.receive()
            XCTAssertEqual(answer.header["match"], .boolean(true))
            await wire.close()
            return transcript
        }
        let pending = try await startingClient.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
        let outcome = try await pending.confirmMatch(true)
        XCTAssertEqual(outcome, .checkStatus)
        let transcript = try await target.value
        let data = try XCTUnwrap(fixture.store.load(scope: fixture.scope))
        let fields = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(Set(fields.keys), ["windowID", "generation", "transcriptHash", "targetKey", "matched"])
        let receipt = try XCTUnwrap(FedRemotePairReceipt.load(data))
        XCTAssertEqual(receipt.transcriptHash, transcript)

        let restartedClient = try fixture.client(pipes: [status])
        let hasReceipt = try await restartedClient.hasRemotePairingReceipt(rendezvous: fixture.rendezvous)
        XCTAssertTrue(hasReceipt)
        let machineID = "fedcba9876543210fedcba9876543210"
        let statusTarget = Task {
            let (wire, _) = try await fixture.targetWire(FedTCPRecordCarrier(stream: status.target), status: true)
            let request = try await wire.receive()
            XCTAssertEqual(request, receipt.request)
            try await wire.send(fixture.result("granted", machineID: machineID))
        }
        let resumed = try await restartedClient.remotePairingStatus(rendezvous: fixture.rendezvous)
        if case .granted(let grant) = resumed {
            XCTAssertEqual(grant.computerStaticPublicKey, fixture.targetKey.publicKey)
            XCTAssertEqual(grant.computerMachineID, machineID)
            XCTAssertEqual(grant.rawName, "Selected Computer")
            XCTAssertEqual(grant.displayName, "Selected Computer")
        } else { XCTFail("expected a grant from status, got \(resumed)") }
        try await statusTarget.value
        XCTAssertNil(try fixture.store.load(scope: fixture.scope))
        await fixture.rendezvous.disconnect()
    }

    func testRelayResponderRefusesBeforeNoiseWithoutMatchingPendingStart() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true, advertiseDirect: false)
        let dial = fixture.dial()
        let wrong = FedRemotePairAttempt(targetKey: fixture.deviceKey.publicKey, store: fixture.store, scope: fixture.scope)
        let cancelled = FedRemotePairAttempt(targetKey: fixture.targetKey.publicKey, store: fixture.store, scope: fixture.scope)
        await cancelled.cancel()
        for attempt in [nil, wrong, cancelled] as [FedRemotePairAttempt?] {
            let pipe = RemotePairingPipe()
            do {
                _ = try await dial.respondOnRelay(pipe.device, attempt: attempt)
                XCTFail("unsolicited pairing relay admitted without a matching pending start")
            } catch { XCTAssertEqual(error as? FedRemotePairingError, .unavailable) }
            let writes = await pipe.writes
            let reads = await pipe.reads
            XCTAssertEqual(writes[0], 0)
            XCTAssertEqual(reads[0], 0)
        }
        await fixture.rendezvous.disconnect()
    }

    func testMatchingPendingRelayRejectsForeignNoiseKeyBeforeMessageTwo() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true, advertiseDirect: false)
        let attempt = FedRemotePairAttempt(targetKey: fixture.targetKey.publicKey, store: fixture.store, scope: fixture.scope)
        let foreign = try FedNoiseKeyPair(privateKey: Data(repeating: 0x73, count: 32))
        let initiator = try FedNoiseIKInitiator(staticKey: foreign, pinnedResponderStatic: fixture.deviceKey.publicKey,
                                               prologue: FedRemotePairMode.targetRelay.prologue())
        let pipe = RemotePairingPipe()
        try await pipe.target.sendNoiseMessage(initiator.writeMessage1())
        do {
            _ = try await fixture.dial().respondOnRelay(pipe.device, attempt: attempt)
            XCTFail("foreign valid IK received message 2")
        } catch { XCTAssertEqual(error as? FedNoiseError, .pinnedResponderKeyMismatch) }
        let writes = await pipe.writes
        XCTAssertEqual(writes[0], 0)
        await fixture.rendezvous.disconnect()
    }

    func testPhoneAttestationUsesRemoteDomainAndPrecedesMatch() async throws {
        let fixture = try await RemotePairingFixture()
        let pipe = RemotePairingPipe()
        let proof = RemotePairingProofProvider()
        let client = try fixture.client(pipes: [pipe], provider: proof)
        let target = Task {
            let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
            _ = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe)
            let frame = try await wire.session.receiveTransportPayload()
            let attestation = try RemotePairingVectors.frame(frame, slot: true)
            XCTAssertEqual(FedRemotePairCodec.decodeProof(attestation), RemotePairingProofProvider.proof)
            let hashes = await proof.hashes
            XCTAssertEqual(hashes, [FedRemotePairCrypto.attestationHash(hash)])
            XCTAssertNotEqual(hashes[0], FedAppAttestCodec.pairingClientDataHash(handshakeHash: hash))
            let answer = try await wire.receive()
            XCTAssertEqual(answer.typeName, FedRemotePairCodec.match)
            XCTAssertEqual(answer.header["match"], .boolean(false))
        }
        let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
        let result = try await pending.confirmMatch(false)
        XCTAssertEqual(result, .notGranted)
        try await target.value
        XCTAssertNil(try FedRemotePairReceipt.load(fixture.store.load(scope: fixture.scope)))
        await fixture.rendezvous.disconnect()
    }

    func testAuthenticatedDirectLegDropsTheProvisionalRelayBeforeCommit() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true)
        let attempt = FedRemotePairAttempt(targetKey: fixture.targetKey.publicKey, store: fixture.store, scope: fixture.scope)
        let relay = RemotePairingPipe(), direct = RemotePairingPipe()
        let dial = fixture.dial()
        let relayTarget = Task { try await fixture.targetWire(relay.target, mode: .targetRelay) }
        let held = try await dial.respondOnRelay(relay.device, attempt: attempt)
        _ = try await relayTarget.value
        let losing = Task {
            try await attempt.exchange(held, dial: dial, deviceClass: .phone, provider: nil, resultTimeout: .seconds(2))
        }
        // Do not let the relay send commit. It has authenticated message 1 and
        // is holding the attempt, not owning it.
        try await fedTestWithTimeout {
            while await relay.reads[0] < 2 { await Task.yield() }
        }
        let directTarget = Task {
            let (wire, _) = try await fixture.targetWire(direct.target)
            try await wire.send(RemotePairingFixture.binding())
            let commit = try await wire.receive()
            XCTAssertEqual(commit.typeName, FedRemotePairCodec.commit)
            let relayClosed = await relay.closed
            XCTAssertTrue(relayClosed, "direct wins before it may commit")
            await wire.close()
        }
        let initiator = try FedNoiseIKInitiator(staticKey: fixture.deviceKey, pinnedResponderStatic: fixture.targetKey.publicKey,
                                               prologue: FedRemotePairMode.direct.prologue())
        let session = try await initiator.establish(on: direct.device, clock: SystemFedMonotonicClock(), timeout: .seconds(2))
        let winner = FedRemotePairLink(wire: FedRemotePairWire(session), handshakeHash: initiator.handshakeHash,
                                       authenticatedTarget: fixture.targetKey.publicKey, mode: .direct)
        do {
            _ = try await attempt.exchange(winner, dial: dial, deviceClass: .phone, provider: nil, resultTimeout: .seconds(2))
            XCTFail("the peer stopped before nonce disclosure")
        } catch { }
        do {
            _ = try await losing.value
            XCTFail("the displaced relay returned a pairing prompt")
        } catch { }
        try await directTarget.value
        let relayWrites = await relay.writes
        XCTAssertEqual(relayWrites[0], 1, "D sends message 2 but discloses nothing on the displaced hold")
        await fixture.rendezvous.disconnect()
    }

    func testDeviceRelayOpenUsesOnlyTheGrantCorrelatedToItsSequence() async throws {
        let fixture = try await RemotePairingFixture(advertiseDirect: false)
        let harness = RelayOpsHarness(client: fixture.rendezvous, server: fixture.rendezvousServer)
        let waiting = Task {
            try await fixture.rendezvous.remotePairRelay(peer: fixture.targetKey.publicKey.lowercaseHex,
                openNonce: String(repeating: "a", count: 32), clock: SystemFedMonotonicClock(), timeout: .seconds(2))
        }
        let open = try await harness.awaitRelayOpen()
        XCTAssertEqual(open.to, fixture.targetKey.publicKey.lowercaseHex)
        try await pushGrant(fixture, serverSeq: "2", ofSeq: "1", side: .a, pipeID: String(repeating: "1", count: 26))
        let expected = String(repeating: "2", count: 26)
        try await pushGrant(fixture, serverSeq: "3", ofSeq: open.seq, side: .a, pipeID: expected)
        let (candidate, _) = try await waiting.value
        XCTAssertEqual(candidate.pipeID, expected)
        await fixture.rendezvous.disconnect()
    }

    func testHigherDeviceKeyDoesNotOpenRelayToCheckStatus() async throws {
        let fixture = try await RemotePairingFixture(targetLower: true, advertiseDirect: false)
        let window = RemotePairingFixture.window(.phone)
        let receipt = FedRemotePairReceipt(windowID: window.id, generation: window.generation,
            transcriptHash: Data(repeating: 0x83, count: 32), targetKey: fixture.targetKey.publicKey, matched: true)
        try fixture.store.replace(scope: fixture.scope, record: JSONEncoder().encode(receipt))
        let client = try fixture.client(pipes: [], upgrade: { _ in
            XCTFail("status cannot use a T-opened relay")
            throw FedRemotePairingError.statusUnavailable
        })
        do {
            _ = try await client.remotePairingStatus(rendezvous: fixture.rendezvous)
            XCTFail("status without an initiator route must be unavailable")
        } catch { XCTAssertEqual(error as? FedRemotePairingError, .statusUnavailable) }
        let unsolicitedWrite = await fixture.rendezvousServer.tryReceiveNow()
        XCTAssertNil(unsolicitedWrite)
        XCTAssertEqual(try FedRemotePairReceipt.load(fixture.store.load(scope: fixture.scope)), receipt)
        await fixture.rendezvous.disconnect()
    }

    private func pushGrant(_ fixture: RemotePairingFixture, serverSeq: String, ofSeq: String?,
                           side: FedRelaySide, pipeID: String) async throws {
        let now = UInt64(Date().timeIntervalSince1970 * 1_000)
        let token = FedPublicTestSupport.pipeTokenWireText(pipeID: pipeID, side: side,
            deviceX25519PublicKey: fixture.deviceKey.publicKey, tokenVersion: 1, expiresAtMs: now + 60_000)
        var fields: [String: RdvJSONValue] = [
            "type": .string("relay_grant"), "server_seq": .string(serverSeq), "side": .string(side.rawValue),
            "pipe_id": .string(pipeID), "relay_url": .string("wss://rdv.test.invalid/v1/pipe/\(pipeID)"),
            "pipe_token": .string(token), "peer": .string(fixture.targetKey.publicKey.lowercaseHex),
            "issued_at_ms": .string(String(now)), "expires_at_ms": .string(String(now + 60_000)),
        ]
        if let ofSeq { fields["of_seq"] = .string(ofSeq) }
        try await fixture.rendezvousServer.send(.text(try RdvCanonicalJSON.canonicalString(.object(RdvJSONObject(fields)))))
    }
}

private actor RemotePairingProofProvider: FedAppAttestProvider {
    static let proof = FedPairingAttestation(keyID: Data(repeating: 0x33, count: 32), attestation: Data([0xA0]))
    private(set) var hashes: [Data] = []

    func attestPairing(clientDataHash: Data) -> FedPairingAttestation {
        hashes.append(clientDataHash)
        return Self.proof
    }

    func sessionAssertion(clientDataHash: Data) throws -> Data {
        XCTFail("pairing cannot use a session assertion")
        return Data()
    }
}
