import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Proves the rdv-wire vectors in this package are still the bytes the cloud
/// plane produces.
///
/// The vectors are consumed by this client and separately by the TypeScript
/// rendezvous worker and the Rust cloud vocabulary. Reading a copy makes them a
/// SNAPSHOT: it records what the producer emitted once, and a later change on
/// the producing side breaks nothing here — the divergence surfaces at a live
/// handshake instead of at a build.
///
/// These tests turn that snapshot into a pin. The digests are written HERE
/// rather than computed from the files: computing them would let any
/// regenerated copy satisfy the test automatically, which is the same hole one
/// level up.
final class RdvVectorCurrencyTests: XCTestCase {

    /// SHA-256 of each vector as published by the producer. A failure means the
    /// contract moved and this client has not caught up — read the diff before
    /// updating, because the vectors are the contract, not a cache of it.
    private static let expectedDigests: [String: String] = [
        "admission-facts.jsonl": "b697bfb45b7328d471039ef29b52fcd93a1527d56c9514a1c9870a200cbf87b1",
        "candidate-record.jsonl": "b64aca180ecb528fc06d349bcbbf4465d4059a6e6dd688f8a6a9af0333682f8c",
        "canonical-valid.jsonl": "dc63a68a2ffd500651ba37946d743abb2f069c1f90ab5379857d52fad739fddc",
        "decimal-string.jsonl": "61e658691267c7f3b528fdc6b1c87c4fc16ab803a203dfbb2d5c10fae4b8b2c1",
        "device-record.jsonl": "78680f32dec61fd98b504ce5a38f6db7fd5ea4ea499a0c36b0d2479c25f04dad",
        "epoch-push-claims.jsonl": "3a04398e882d4dec44e7c1f65f56976342ec3c4cf3d6734aae2b74088d21883a",
        "nesting-depth.jsonl": "deefbb3e6efdc7cc6d677f862711e2d00483c1acbcd3da3724cb548e8b5969a6",
        "parse-reject.jsonl": "c7fcc91cd4c39319331bdd69e00dcdfd3294216eb2be65d84c50e1edde9e0cb2",
        "pipe-token.jsonl": "c9c061a52f3dcb9896b13a1827b7e07c4b2db4330123d99611fb635920415996",
        "rust-signed.jsonl": "2926ad2c6af8245fcaa8646bf3f972a9769e31c73103369ac9a42e898a785959",
        "ts-signed.jsonl": "917257b5c4429667e2c5ca5310fbbebc6129f4f9517514e2d9089de92e7782ba",
        "device-record-key.json": "9c850d8c6812995fb5c141278f7d7606290f1fa16e817c7607a0c57e78224c6d",
        "signing-key.json": "f530ca14054fd3e333a78ea65ca34d9fe7791e14d986385129b35c789fe77b36",
    ]

    func testEveryVectorMatchesThePublishedBytes() throws {
        for (filename, expected) in Self.expectedDigests {
            let url = URL(fileURLWithPath: RdvWireFixtures.directory).appendingPathComponent(filename)
            let data = try Data(contentsOf: url)
            let actual = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
            XCTAssertEqual(actual, expected, """
                \(filename) does not match the published rdv-wire vector. \
                Either the cloud contract changed and this client must catch up, \
                or the local copy was edited — the vectors are the contract, so \
                do not update this digest without reading what moved.
                """)
        }
    }

    /// A path that resolves to nothing would make every other vector assertion
    /// pass by iterating an empty set, so the directory's contents are asserted
    /// rather than assumed.
    func testTheVectorDirectoryIsPopulated() throws {
        let files = try FileManager.default.contentsOfDirectory(atPath: RdvWireFixtures.directory)
        let vectors = files.filter { $0.hasSuffix(".jsonl") || $0.hasSuffix(".json") }
        XCTAssertEqual(
            Set(vectors), Set(Self.expectedDigests.keys),
            "the vector set changed; a vector added or removed upstream is a contract change")
    }
}
