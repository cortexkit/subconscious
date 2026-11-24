import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

enum RemotePairingVectors {
    typealias Object = [String: Any]

    static func resource(_ file: String) throws -> Data {
        let directory = try XCTUnwrap(Bundle.module.resourceURL).appendingPathComponent("remote-pairing-v1")
        return try Data(contentsOf: directory.appendingPathComponent(file))
    }

    static func json(_ file: String) throws -> Object {
        try XCTUnwrap(JSONSerialization.jsonObject(with: resource(file)) as? Object)
    }

    static func hex(_ value: Any?) throws -> Data { try Data(hex: XCTUnwrap(value as? String)) }

    static func frame(_ raw: Data, slot: Bool = false) throws -> FedFrame {
        var parser = FedRemotePairCodec.decoder(attestationSlot: slot)
        var frames: [FedFrame] = []
        // Prefixes, headers and bodies need not share a Noise record.
        for index in stride(from: 0, to: raw.count, by: 97) {
            frames += try parser.append(raw.subdata(in: index..<min(index + 97, raw.count)))
        }
        if frames.count > 1 || (!frames.isEmpty && parser.hasPartialFrame) { throw FedRemotePairCodec.Rejection.trailing }
        try parser.finish()
        guard let frame = frames.first else { throw FedRemotePairCodec.Rejection.truncated }
        return frame
    }
}

final class RemotePairingVectorTests: XCTestCase {
    func testPublishedChecksums() throws {
        let bytes = try RemotePairingVectors.resource("SHA256SUMS")
        XCTAssertEqual(Data(SHA256.hash(data: bytes)).lowercaseHex,
                       "e53ab4dd96da5ccd7fdd7ed53b31209563867e329364c3b60f6f680bf2ce0cb2")
        var names = Set<String>()
        for line in String(decoding: bytes, as: UTF8.self).split(separator: "\n") {
            let columns = line.split(separator: " ").map(String.init)
            XCTAssertEqual(columns.count, 2)
            let file = try XCTUnwrap(columns.last)
            names.insert(file)
            XCTAssertEqual(try Data(SHA256.hash(data: RemotePairingVectors.resource(file))).lowercaseHex, columns.first, file)
        }
        XCTAssertEqual(names, ["README.md", "cases.json", "check.py", "state-machine.json"])
    }

    func testEveryDerivation() throws {
        let cases = try XCTUnwrap(RemotePairingVectors.json("cases.json")["derivations"] as? [RemotePairingVectors.Object])
        XCTAssertEqual(cases.count, 3)
        for sample in cases {
            func raw(_ name: String) throws -> Data { try RemotePairingVectors.hex(sample[name]) }
            let device = try raw("device_key"), target = try raw("target_key")
            let mode: FedRemotePairMode = sample["leg"] as? String == "direct" ? .direct : try .relay(device: device, target: target)
            XCTAssertEqual(Int(mode.rawValue), sample["mode"] as? Int)
            XCTAssertEqual(try mode.prologue(), try raw("pair_prologue"))
            if mode == .targetRelay { XCTAssertThrowsError(try mode.prologue(status: true)) }
            else { XCTAssertEqual(try mode.prologue(status: true), try raw("status_prologue")) }
            let window = FedRemotePairWindow(id: try raw("window_id"),
                generation: try XCTUnwrap(UInt64(try XCTUnwrap(sample["generation"] as? String))),
                deviceClass: try XCTUnwrap(FedRemotePairingClass(rawValue: try XCTUnwrap(sample["class"] as? String))),
                confirmerKey: try raw("confirmer_key"), confirmerName: try XCTUnwrap(sample["confirmer_name"] as? String))
            let handshake = try raw("handshake_hash")
            let context = window.context(device: device, target: target, mode: mode, handshakeHash: handshake)
            XCTAssertEqual(context, try raw("context"))
            let commit = try FedRemotePairCrypto.commit(context: context, initiator: raw("nonce_i"))
            XCTAssertEqual(commit, try raw("commit"))
            let transcript = try FedRemotePairCrypto.transcript(context: context, commit: commit,
                                                               responder: raw("nonce_r"), initiator: raw("nonce_i"))
            XCTAssertEqual(transcript, try raw("transcript_hash"))
            let digest = FedRemotePairCrypto.codeDigest(transcript: transcript)
            XCTAssertEqual(digest, try raw("code_digest"))
            let code = FedRemotePairCrypto.code(digest: digest)
            XCTAssertEqual(code, sample["code"] as? String)
            XCTAssertEqual(FedRemotePairCrypto.printedCode(code), sample["printed_code"] as? String)
            XCTAssertEqual(FedRemotePairCrypto.attestationHash(handshake), try raw("attestation_client_data_hash"))
        }
    }

    func testEveryCanonicalAndRejectedFrame() throws {
        let root = try RemotePairingVectors.json("cases.json")
        let frames = try XCTUnwrap(root["frames"] as? [String: RemotePairingVectors.Object])
        XCTAssertEqual(frames.count, 44)
        for (name, sample) in frames {
            let bytes = try RemotePairingVectors.hex(sample["frame_hex"])
            let expected = try FedJSONObject(jsonData: Data(XCTUnwrap(sample["header"] as? String).utf8))
            let decoded = try RemotePairingVectors.frame(bytes, slot: name == "attestation")
            XCTAssertEqual(decoded.header, expected, name)
            XCTAssertEqual(try FedRemotePairCodec.encode(FedFrame(header: expected, body: Data())), bytes, name)
        }
        let rejected = try XCTUnwrap(root["rejected"] as? [RemotePairingVectors.Object])
        XCTAssertEqual(rejected.count, 29)
        for sample in rejected {
            let bytes = try RemotePairingVectors.hex(sample["frame_hex"])
            XCTAssertThrowsError(try RemotePairingVectors.frame(bytes), String(describing: sample["name"])) {
                XCTAssertEqual(FedRemotePairCodec.rejection($0).rawValue, sample["error"] as? String, String(describing: sample["name"]))
            }
        }
    }

    func testGrantedResultRoundTripsMachineIDInMemberOrder() throws {
        let machineID = "0123456789abcdef0123456789abcdef"
        let bytes = resultBytes(#","machine_id":"\#(machineID)""#)
        let decoded = try RemotePairingVectors.frame(bytes)
        XCTAssertEqual(decoded.header["machine_id"], .string(machineID))
        XCTAssertEqual(try FedRemotePairCodec.encode(decoded), bytes)
    }

    func testResultKeepsStrictMachineIDValidationAndMemberSets() throws {
        let validID = "0123456789abcdef0123456789abcdef"
        for value in [#""abcdef""#, #""0123456789ABCDEF0123456789abcdef""#,
                      #""g123456789abcdef0123456789abcdef""#, "7", "null"] {
            XCTAssertThrowsError(try RemotePairingVectors.frame(resultBytes(#","machine_id":\#(value)"#)), value) {
                XCTAssertEqual(FedRemotePairCodec.rejection($0), .badField, value)
            }
        }
        XCTAssertThrowsError(try RemotePairingVectors.frame(resultBytes(#","machine_id":"\#(validID)""#, granted: false))) {
            XCTAssertEqual(FedRemotePairCodec.rejection($0), .members)
        }
        XCTAssertThrowsError(try RemotePairingVectors.frame(resultBytes(#","machine_id":"\#(validID)","unknown":true"#))) {
            XCTAssertEqual(FedRemotePairCodec.rejection($0), .members)
        }
    }

    private func resultBytes(_ extraMembers: String, granted: Bool = true) -> Data {
        let key = String(repeating: "ab", count: 32)
        let fields = granted ? #""outcome":"granted","target_key":"\#(key)""# : #""outcome":"not_granted""#
        let header = Data(#"{"type":"remote_pair_result",\#(fields)\#(extraMembers)}"#.utf8)
        let prefix = withUnsafeBytes(of: UInt32(header.count).littleEndian) { Data($0) }
        return prefix + header + Data(repeating: 0, count: 4)
    }

    func testEveryProofSlotSequence() throws {
        let root = try RemotePairingVectors.json("cases.json")
        let samples = try XCTUnwrap(root["attestation_cases"] as? [RemotePairingVectors.Object])
        XCTAssertEqual(samples.count, 11)
        for sample in samples {
            var decoder = FedRemotePairCodec.decoder(attestationSlot: try XCTUnwrap(sample["slot"] as? Bool))
            var observed: [String] = []
            for raw in try XCTUnwrap(sample["frames_hex"] as? [String]) {
                do {
                    let frames = try decoder.append(Data(hex: raw))
                    try decoder.finish()
                    let frame = try XCTUnwrap(frames.first)
                    if frame.typeName == FedRemotePairCodec.proof {
                        observed.append(FedRemotePairCodec.decodeProof(frame) == nil ? "InvalidAttestation" : "Attestation")
                    } else {
                        XCTAssertEqual(frame.typeName, FedRemotePairCodec.match)
                        observed.append("Match")
                    }
                } catch { observed.append(FedRemotePairCodec.rejection(error).rawValue); break }
            }
            XCTAssertEqual(observed.joined(separator: ","), sample["expected"] as? String, String(describing: sample["name"]))
        }
    }
}
