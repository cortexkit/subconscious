import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

enum PairingVectors {
    typealias Object = [String: Any]
    static let manifestSHA256 = "c158b9db12d39aab1ade7869774e430da8302feb3db2a7f70a0e41645c82889c"
    static func url(_ name: String) throws -> URL {
        try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: nil, subdirectory: "fed-pairing-v1"))
    }
    static func load(_ name: String) throws -> Object {
        try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url(name))) as? Object)
    }
    static func hex(_ text: Any?) throws -> Data {
        try Data(hex: XCTUnwrap(text as? String))
    }
    static func object(_ value: Any?) throws -> Object { try XCTUnwrap(value as? Object) }
    static func cases(_ root: Object, _ key: String) throws -> [Object] { try XCTUnwrap(root[key] as? [Object]) }
    static func handshake(_ id: String) throws -> Object {
        try XCTUnwrap(cases(load("handshakes.json"), "positive_cases").first { $0["id"] as? String == id })
    }
}

final class PairingVectorTests: XCTestCase {
    func testVendoredFilesMatchTaggedSHA256SUMS() throws {
        let manifest = try Data(contentsOf: PairingVectors.url("SHA256SUMS"))
        XCTAssertEqual(Data(SHA256.hash(data: manifest)).lowercaseHex, PairingVectors.manifestSHA256)
        for line in String(decoding: manifest, as: UTF8.self).split(separator: "\n") {
            let fields = line.split(separator: " ", maxSplits: 1, omittingEmptySubsequences: true)
            let name = fields[1].trimmingCharacters(in: .whitespaces)
            let data = try Data(contentsOf: PairingVectors.url(name))
            XCTAssertEqual(Data(SHA256.hash(data: data)).lowercaseHex, String(fields[0]), name)
        }
    }

    func testProductionWordlistDigest() throws {
        let data = try Data(contentsOf: FedPairingCrypto.wordlistURL)
        XCTAssertEqual(Data(SHA256.hash(data: data)).lowercaseHex,
                       "2f5eed53a4727b4bf8880d8f3f199efc90e58503646d9ff8eff3a2ed3b24dbda")
        XCTAssertEqual(FedPairingCrypto.wordlist.count, 2048)
    }

    func testHandshake_one_bit_a() throws { try checkHandshake("one_bit_a") }
    func testHandshake_one_bit_b() throws { try checkHandshake("one_bit_b") }
    func testHandshake_distinct_inputs() throws { try checkHandshake("distinct_inputs") }

    private func checkHandshake(_ id: String) throws {
        let v = try PairingVectors.handshake(id)
        let si = try PairingVectors.object(v["initiator_static"])
        let sr = try PairingVectors.object(v["responder_static"])
        let ei = try PairingVectors.object(v["initiator_ephemeral"])
        let psk = try FedPairingCrypto.derivePSK(secret: PairingVectors.hex(v["secret"]),
                                               computerStaticPublicKey: PairingVectors.hex(sr["public"]))
        let state = try FedNoiseIKpsk1Initiator(staticKey: FedNoiseKeyPair(privateKey: PairingVectors.hex(si["private"])),
                                               responderStatic: PairingVectors.hex(sr["public"]), psk: psk)
        let m1 = try state.writeMessage1(using: FedFixedNoiseEntropy(PairingVectors.hex(ei["private"])))
        XCTAssertEqual(m1.count, 96)
        XCTAssertEqual(m1, try PairingVectors.hex(v["message1"]))
        let result = try state.readMessage2(PairingVectors.hex(v["message2"]))
        XCTAssertTrue(state.isComplete)
        XCTAssertEqual(result.handshakeHash, try PairingVectors.hex(v["handshake_hash"]))
        let keys = try result.takeTransportMaterial()
        let split = try PairingVectors.object(v["split"])
        XCTAssertEqual(keys.sendKey, try PairingVectors.hex(split["initiator_to_responder"]))
        XCTAssertEqual(keys.receiveKey, try PairingVectors.hex(split["responder_to_initiator"]))
        XCTAssertEqual(try FedPairingCrypto.confirmationWords(handshakeHash: result.handshakeHash),
                       try PairingVectors.object(v["confirmation"])["words"] as? [String])
    }

    func testPSKAndThreeOutputHKDFVectors() throws {
        for v in try PairingVectors.cases(PairingVectors.load("handshakes.json"), "positive_cases") {
            let p = try PairingVectors.object(v["psk_derivation"])
            let extract = FedBLAKE2s.hmac(key: try PairingVectors.hex(p["salt"]), message: try PairingVectors.hex(p["ikm"]))
            XCTAssertEqual(extract, try PairingVectors.hex(p["extract"]))
            XCTAssertEqual(try FedPairingCrypto.derivePSK(secret: PairingVectors.hex(p["ikm"]),
                                                         computerStaticPublicKey: PairingVectors.hex(p["salt"])),
                           try PairingVectors.hex(p["psk"]))
            for h in try PairingVectors.cases(v, "mix_key_and_hash") {
                let ck = try PairingVectors.hex(h["chaining_key_before"])
                let psk = try PairingVectors.hex(h["psk"])
                XCTAssertEqual(FedBLAKE2s.hmac(key: ck, message: psk), try PairingVectors.hex(h["extract"]))
                let outputs = FedBLAKE2s.hkdf(chainingKey: ck, inputKeyMaterial: psk, outputCount: 3)
                XCTAssertEqual(outputs, try ["output1_ck", "output2_temp_h", "output3_temp_k"].map { try PairingVectors.hex(h[$0]) })
            }
        }
    }

    func testPlainIKPublishedControl() throws {
        let v = try PairingVectors.object(PairingVectors.load("handshakes.json")["plain_ik_control"])
        let si = try PairingVectors.object(v["initiator_static"])
        let sr = try PairingVectors.object(v["responder_static"])
        let ei = try PairingVectors.object(v["initiator_ephemeral"])
        let state = try FedNoiseIKInitiator(staticPrivateKey: PairingVectors.hex(si["private"]), pinnedResponderStatic: PairingVectors.hex(sr["public"]))
        XCTAssertEqual(try state.writeMessage1(using: FedFixedNoiseEntropy(PairingVectors.hex(ei["private"]))), try PairingVectors.hex(v["message1"]))
        let result = try state.readMessage2(PairingVectors.hex(v["message2"]))
        XCTAssertEqual(result.handshakeHash, try PairingVectors.hex(v["handshake_hash"]))
        let keys = try result.takeTransportMaterial()
        let split = try PairingVectors.object(v["split"])
        XCTAssertEqual(keys.sendKey, try PairingVectors.hex(split["initiator_to_responder"]))
        XCTAssertEqual(keys.receiveKey, try PairingVectors.hex(split["responder_to_initiator"]))
    }

    func testAllWordsVectors() throws {
        let root = try PairingVectors.load("words.json")
        var cases = try PairingVectors.cases(root, "positive_cases")
        cases.append(try PairingVectors.object(root["hand_built_handshake_hash"]))
        for v in cases {
            let hash = try PairingVectors.hex(v["handshake_hash"])
            let digest = FedBLAKE2s.hash(Data("ck-fed-pair-words-v1".utf8) + hash)
            XCTAssertEqual(digest, try PairingVectors.hex(v["digest"]))
            XCTAssertEqual(FedPairingCrypto.indices(digest: digest), v["indices"] as? [Int])
            XCTAssertEqual(try FedPairingCrypto.confirmationWords(handshakeHash: hash), v["words"] as? [String])
        }
        let extraction = try PairingVectors.object(root["extraction_only"])
        let digest = try PairingVectors.hex(extraction["digest"])
        XCTAssertEqual(FedPairingCrypto.indices(digest: digest), extraction["indices"] as? [Int])
        XCTAssertEqual(FedPairingCrypto.words(digest: digest), extraction["words"] as? [String])
    }
}
