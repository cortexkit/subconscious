import Foundation
import XCTest
@testable import SubcFed

private actor AttestingPairingStream: FedTCPByteStream {
    let vector: PairingVectors.Object
    let resultBytes: Data?
    private var pending: [Data] = []
    private(set) var writes: [Data] = []
    private(set) var closed = false

    init(resultBytes: Data?) throws {
        vector = try PairingVectors.handshake("one_bit_a")
        self.resultBytes = resultBytes
    }

    func write(_ bytes: Data) throws {
        writes.append(bytes)
        if writes.count == 1 {
            let expected = try FedOuterRecordCodec.encode(PairingVectors.hex(vector["message1"]))
            XCTAssertEqual(bytes, expected)
            guard bytes == expected else { throw FedNoiseError.invalidMessage }
            pending.append(try FedOuterRecordCodec.encode(PairingVectors.hex(vector["message2"])))
            if let resultBytes {
                let split = try PairingVectors.object(vector["split"])
                let ciphertext = try FedNoiseChaChaPoly.seal(
                    resultBytes,
                    key: PairingVectors.hex(split["responder_to_initiator"]),
                    nonce: 0,
                    authenticatedData: Data()
                )
                pending.append(try FedOuterRecordCodec.encode(ciphertext))
            }
            return
        }
        XCTAssertEqual(writes.count, 2, "pairing must send at most one attestation frame")
    }

    func read() async throws -> Data? {
        if !pending.isEmpty { return pending.removeFirst() }
        if closed { return nil }
        try await Task.sleep(nanoseconds: 50_000_000)
        return nil
    }

    func close() {
        closed = true
    }
}

private actor AttestingConnectRecorder {
    var endpoints: [String] = []
    let stream: AttestingPairingStream
    init(stream: AttestingPairingStream) { self.stream = stream }
    func connect(host: String, port: UInt16) throws -> any FedTCPByteStream {
        endpoints.append("\(host):\(port)")
        return stream
    }
}

final class FedAppAttestPairingTests: XCTestCase {
    private func pairingQR() throws -> FedPairingQR {
        let root = try PairingVectors.load("qr.json")
        let vector = try XCTUnwrap(PairingVectors.cases(root, "cases").first { $0["id"] as? String == "valid_four_hints" })
        return try FedPairingQR.parse(XCTUnwrap(vector["qr"] as? String), now: Date(timeIntervalSince1970: 2_000_000_000))
    }

    private func pairingClient(
        stream: AttestingPairingStream,
        recorder: AttestingConnectRecorder,
        provider: (any FedAppAttestProvider)?
    ) throws -> SubcFedClient {
        let vector = try PairingVectors.handshake("one_bit_a")
        let initiatorStatic = try PairingVectors.object(vector["initiator_static"])
        let initiatorEphemeral = try PairingVectors.object(vector["initiator_ephemeral"])
        return SubcFedClient(
            profile: try FedPublicTestSupport.humanProfile(),
            keyStore: try FedMemoryPrivateKeyStore(noisePrivateKey: PairingVectors.hex(initiatorStatic["private"])),
            stateStore: FedMemoryStateStore(),
            observedNetwork: {
                XCTFail("pairing must not consult normal dial candidates")
                return FedObservedNetworkSnapshot()
            },
            clock: SystemFedMonotonicClock(),
            entropy: FedFixedNoiseEntropy(try PairingVectors.hex(initiatorEphemeral["private"]) + PairingVectors.hex(initiatorEphemeral["private"])),
            pairingDialFactory: FedNetworkDialFactory(connect: { try await recorder.connect(host: $0, port: $1) }),
            appAttestProvider: provider
        )
    }

    private static func rawFrame(_ header: String, body: Data = Data()) -> Data {
        func length(_ count: Int) -> Data {
            var little = UInt32(count).littleEndian
            return withUnsafeBytes(of: &little) { Data($0) }
        }
        let bytes = Data(header.utf8)
        return length(bytes.count) + bytes + length(body.count) + body
    }

    func testFailingPairingProviderNeverFailsPairing() async throws {
        let stream = try AttestingPairingStream(resultBytes: Self.rawFrame("{\"type\":\"pairing_result\",\"outcome\":\"granted\"}"))
        let recorder = AttestingConnectRecorder(stream: stream)
        let provider = FakeAppAttestProvider(pairingResult: .failure(FakeAttestError()))
        let client = try pairingClient(stream: stream, recorder: recorder, provider: provider)
        let pending = try await client.pair(qr: pairingQR())
        let grant = try await pending.result()
        XCTAssertFalse(grant.computerStaticPublicKey.isEmpty)
        XCTAssertEqual(provider.recordedPairingCalls().count, 1)
        let writes = await stream.writes
        XCTAssertEqual(writes.count, 1, "a failed attestation sends nothing")
    }

    func testMissingPairingProviderSendsNothing() async throws {
        let stream = try AttestingPairingStream(resultBytes: Self.rawFrame("{\"type\":\"pairing_result\",\"outcome\":\"granted\"}"))
        let recorder = AttestingConnectRecorder(stream: stream)
        let client = try pairingClient(stream: stream, recorder: recorder, provider: nil)
        let pending = try await client.pair(qr: pairingQR())
        _ = try await pending.result()
        let writes = await stream.writes
        XCTAssertEqual(writes.count, 1)
    }

    func testSuccessfulPairingProviderSendsSpecAttestation() async throws {
        let keyID = Data([0xAA, 0xBB, 0xCC])
        let attestation = Data([0x01, 0x02, 0x03, 0x04])
        let stream = try AttestingPairingStream(resultBytes: Self.rawFrame("{\"type\":\"pairing_result\",\"outcome\":\"granted\"}"))
        let recorder = AttestingConnectRecorder(stream: stream)
        let provider = FakeAppAttestProvider(
            pairingResult: .success(FedPairingAttestation(keyID: keyID, attestation: attestation))
        )
        let client = try pairingClient(stream: stream, recorder: recorder, provider: provider)
        let pending = try await client.pair(qr: pairingQR())
        _ = try await pending.result()

        let writes = await stream.writes
        XCTAssertEqual(writes.count, 2, "one Noise message plus one attestation frame")
        let vector = try PairingVectors.handshake("one_bit_a")
        let split = try PairingVectors.object(vector["split"])
        let sendKey = try PairingVectors.hex(split["initiator_to_responder"])
        let ciphertext = try FedOuterRecordCodec.decodeTCPRecord(writes[1])
        let plaintext = try FedNoiseChaChaPoly.open(ciphertext, key: sendKey, nonce: 0, authenticatedData: Data())
        let decoded = try FedFrameCodec.decode(plaintext, negotiationComplete: false)
        let expected = FedAppAttestCodec.pairingAttestationFrame(keyID: keyID, attestation: attestation)
        XCTAssertEqual(decoded, expected)
        XCTAssertTrue(decoded.body.isEmpty)

        let handshakeHash = try PairingVectors.hex(vector["handshake_hash"])
        XCTAssertEqual(
            provider.recordedPairingCalls(),
            [FedAppAttestCodec.pairingClientDataHash(handshakeHash: handshakeHash)]
        )
    }
}
