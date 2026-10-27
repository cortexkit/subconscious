import Foundation
import XCTest
@testable import SubcFed

final class FedAppAttestSessionTests: XCTestCase {
    private var localKey: Data { Data(repeating: 0x11, count: 32) }
    private var responderKey: Data { Data(repeating: 0x22, count: 32) }
    private var incarnation: String { "00000000-0000-4000-8000-0000000000aa" }
    private var ledgerEpoch: String { "00000000-0000-4000-8000-0000000000bb" }

    private func remoteHelloFrame() throws -> FedFrame {
        FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: ["mgmt-v1", "effects-v1"]),
            incarnation: incarnation,
            ledgerEpoch: ledgerEpoch,
            connectionAttemptID: String(repeating: "d", count: 32)
        )
    }

    private func remoteCatalogFrame(modulesJSON: String = "{\"modules\":[]}") -> FedFrame {
        FedFrame(
            type: FedFrameType.catalog.rawValue,
            fields: ["generation": .integer(1)],
            body: Data(modulesJSON.utf8)
        )
    }

    private func feed(_ transport: FedLoopbackByteTransport, frame: FedFrame, complete: Bool) async throws {
        let bytes = try FedFrameCodec.encode(
            frame, negotiationComplete: complete,
            negotiatedFeatures: complete ? ["mgmt-v1", "effects-v1"] : []
        )
        await transport.enqueueInbound(bytes)
    }

    private func waitUntil(_ predicate: @escaping () async throws -> Bool) async throws {
        let start = DispatchTime.now().uptimeNanoseconds
        while try await !predicate() {
            guard DispatchTime.now().uptimeNanoseconds - start < 2_000_000_000 else {
                XCTFail("timeout waiting for session progress")
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }

    private func establish(
        transport: FedLoopbackByteTransport,
        handshakeHash: Data?,
        provider: (any FedAppAttestProvider)?
    ) async throws -> FedSessionEngine {
        let engine = FedSessionEngine(deps: .init(
            transport: transport,
            store: FedMemoryStateStore(),
            clock: FedFakeClock(),
            localPublicKey: localKey,
            responderStaticPublicKey: responderKey,
            helloPolicy: try FedHelloPolicy(),
            connectionAttemptID: String(repeating: "c", count: 32),
            handshakeHash: handshakeHash,
            appAttestProvider: provider
        ))
        let task = Task { try await engine.establish() }
        try await waitUntil {
            try await transport.sentFrames(negotiationComplete: false).contains { $0.knownType == .hello }
        }
        try await feed(transport, frame: remoteHelloFrame(), complete: false)
        try await waitUntil {
            let sent = try await transport.sentFrames(negotiationComplete: true, features: ["mgmt-v1", "effects-v1"])
            return sent.contains { $0.knownType == .catalog }
        }
        try await feed(transport, frame: remoteCatalogFrame(), complete: true)
        try await task.value
        return engine
    }

    func testSessionAssertionIsFirstFrameAfterHello() async throws {
        let handshakeHash = Data(repeating: 0x5A, count: 32)
        let expectedClientDataHash = FedAppAttestCodec.sessionClientDataHash(handshakeHash: handshakeHash)
        let assertion = Data("fake-cbor-assertion".utf8)
        let provider = FakeAppAttestProvider(assertionResult: .success(assertion))
        let transport = FedLoopbackByteTransport()
        let engine = try await establish(transport: transport, handshakeHash: handshakeHash, provider: provider)
        let phase = await engine.currentPhase
        XCTAssertEqual(phase, .ready)
        XCTAssertEqual(provider.recordedAssertionCalls(), [expectedClientDataHash])

        let sent = try await transport.sentFrames(negotiationComplete: true, features: ["mgmt-v1", "effects-v1"])
        XCTAssertEqual(sent.count, 3)
        XCTAssertEqual(sent[0].knownType, .hello)
        XCTAssertEqual(sent[1].typeName, FedAppAttestCodec.sessionAssertionType)
        XCTAssertEqual(sent[1].header["assertion"], .string(FedAppAttestCodec.base64URLEncode(assertion)))
        XCTAssertTrue(sent[1].body.isEmpty)
        XCTAssertEqual(sent[2].knownType, .catalog)
        await engine.disconnect()
    }

    func testAbsentProviderSendsNothingAndSessionContinues() async throws {
        let transport = FedLoopbackByteTransport()
        let engine = try await establish(
            transport: transport, handshakeHash: Data(repeating: 0x5A, count: 32), provider: nil
        )
        let phase = await engine.currentPhase
        XCTAssertEqual(phase, .ready)
        let sent = try await transport.sentFrames(negotiationComplete: true, features: ["mgmt-v1", "effects-v1"])
        XCTAssertEqual(sent.map(\.knownType), [.hello, .catalog])
        XCTAssertFalse(sent.contains { $0.typeName == FedAppAttestCodec.sessionAssertionType })
        await engine.disconnect()
    }

    func testFailingProviderSendsNothingAndSessionContinues() async throws {
        let provider = FakeAppAttestProvider(assertionResult: .failure(FakeAttestError()))
        let transport = FedLoopbackByteTransport()
        let engine = try await establish(
            transport: transport, handshakeHash: Data(repeating: 0x5A, count: 32), provider: provider
        )
        let failingPhase = await engine.currentPhase
        XCTAssertEqual(failingPhase, .ready)
        XCTAssertEqual(provider.recordedAssertionCalls().count, 1)
        let sent = try await transport.sentFrames(negotiationComplete: true, features: ["mgmt-v1", "effects-v1"])
        XCTAssertEqual(sent.map(\.knownType), [.hello, .catalog])
        await engine.disconnect()
    }

    func testMissingHandshakeHashSendsNothing() async throws {
        let provider = FakeAppAttestProvider(assertionResult: .success(Data([1, 2, 3])))
        let transport = FedLoopbackByteTransport()
        let engine = try await establish(transport: transport, handshakeHash: nil, provider: provider)
        let missingPhase = await engine.currentPhase
        XCTAssertEqual(missingPhase, .ready)
        XCTAssertTrue(provider.recordedAssertionCalls().isEmpty)
        let sent = try await transport.sentFrames(negotiationComplete: true, features: ["mgmt-v1", "effects-v1"])
        XCTAssertEqual(sent.map(\.knownType), [.hello, .catalog])
        await engine.disconnect()
    }

    func testShareCodecNeverCarriesAssertion() throws {
        XCTAssertFalse(FedShareCodec.frameTypes.contains(FedAppAttestCodec.sessionAssertionType))
        XCTAssertFalse(FedShareCodec.frameTypes.contains(FedAppAttestCodec.pairingAttestationType))
        let hello = try FedShareCodec.hello(policy: FedHelloPolicy(), incarnation: incarnation, ledgerEpoch: ledgerEpoch)
        XCTAssertEqual(hello.header["session_kind"], .string("share"))
    }
}
