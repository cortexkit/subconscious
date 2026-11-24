import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class RemotePairingV2VectorTests: XCTestCase {
    private func resource(_ file: String) throws -> Data {
        let directory = try XCTUnwrap(Bundle.module.resourceURL).appendingPathComponent("remote-pairing-v2")
        return try Data(contentsOf: directory.appendingPathComponent(file))
    }

    private func cases() throws -> RemotePairingVectors.Object {
        try XCTUnwrap(JSONSerialization.jsonObject(with: resource("cases.json")) as? RemotePairingVectors.Object)
    }

    func testPublishedChecksums() throws {
        let bytes = try resource("SHA256SUMS")
        XCTAssertEqual(Data(SHA256.hash(data: bytes)).lowercaseHex,
                       "9f9f5218fb824686a89eed6e68873354de22d88a5af02266a20678ab66fcdee6")
        var names = Set<String>()
        for line in String(decoding: bytes, as: UTF8.self).split(separator: "\n") {
            let columns = line.split(separator: " ").map(String.init)
            XCTAssertEqual(columns.count, 2)
            let file = try XCTUnwrap(columns.last)
            names.insert(file)
            XCTAssertEqual(try Data(SHA256.hash(data: resource(file))).lowercaseHex, columns.first, file)
        }
        XCTAssertEqual(names, ["README.md", "cases.json"])
    }

    func testEveryV2FrameThroughCodecAndSession() async throws {
        let root = try cases()
        XCTAssertEqual(root["version"] as? String, "remote-pairing-v2")
        let frames = try XCTUnwrap(root["frames"] as? [String: RemotePairingVectors.Object])
        XCTAssertEqual(Set(frames.keys), ["granted", "granted_with_machine_id"])
        for (name, sample) in frames.sorted(by: { $0.key < $1.key }) {
            let bytes = try RemotePairingVectors.hex(sample["frame_hex"])
            let expected = try FedJSONObject(jsonData: Data(XCTUnwrap(sample["header"] as? String).utf8))
            let decoded = try RemotePairingVectors.frame(bytes)
            XCTAssertEqual(decoded.header, expected, name)
            XCTAssertEqual(try FedRemotePairCodec.encode(decoded), bytes, name)
            let machineID = name == "granted" ? nil : "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf"
            try await checkSession(bytes, name: name, machineID: machineID, rejected: false)
        }
        let rejected = try XCTUnwrap(root["rejected"] as? [RemotePairingVectors.Object])
        XCTAssertEqual(rejected.count, 5)
        for sample in rejected {
            let name = try XCTUnwrap(sample["name"] as? String)
            let bytes = try RemotePairingVectors.hex(sample["frame_hex"])
            XCTAssertThrowsError(try RemotePairingVectors.frame(bytes), name) {
                XCTAssertEqual(FedRemotePairCodec.rejection($0).rawValue, sample["error"] as? String, name)
            }
            try await checkSession(bytes, name: name, machineID: nil, rejected: true)
        }
    }

    private func checkSession(_ bytes: Data, name: String, machineID: String?, rejected: Bool) async throws {
        let fixture = try await RemotePairingFixture()
        let pipe = RemotePairingPipe()
        let client = try fixture.client(pipes: [pipe])
        // The published public key has no test private key, so bind the session
        // to its own authenticated target while preserving the rest of the frame.
        let vectorKey = Data("808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f".utf8)
        var sessionBytes = bytes
        if let range = sessionBytes.range(of: vectorKey) {
            sessionBytes.replaceSubrange(range, with: Data(fixture.targetKey.publicKey.lowercaseHex.utf8))
        }
        let payload = sessionBytes
        let target = Task {
            let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
            _ = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe)
            let match = try await wire.receive()
            XCTAssertEqual(match.header["match"], .boolean(true), name)
            try await wire.session.sendTransportPayload(payload)
        }
        let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
        do {
            let outcome = try await pending.confirmMatch(true)
            if case .granted(let grant) = outcome, !rejected {
                XCTAssertEqual(grant.computerStaticPublicKey, fixture.targetKey.publicKey, name)
                XCTAssertEqual(grant.computerMachineID, machineID, name)
            } else {
                XCTFail("\(name): unexpected outcome \(outcome)")
            }
        } catch {
            if rejected { XCTAssertEqual(error as? FedRemotePairingError, .protocolViolation, name) }
            else { XCTFail("\(name): unexpected error \(error)") }
        }
        try await target.value
        await fixture.rendezvous.disconnect()
    }
}
