import Foundation
import XCTest
@testable import SubcClient

private struct WireVectors: Decodable {
    let proofVectors: [ProofVector]
    let frameVectors: [FrameVector]
}

private struct ProofVector: Decodable {
    let name: String
    let keyHex: String
    let domain: String
    let clientNonceHex: String
    let serverNonceHex: String
    let daemonIdHex: String
    let expectedProofHex: String
}

private struct FrameVector: Decodable {
    let name: String
    let ty: UInt8
    let flags: UInt8
    let channel: UInt16
    let epoch: UInt32
    let corr: UInt64
    let bodyHex: String
    let expectedHeaderHex: String
    let expectedFrameHex: String
}

private enum FixtureError: Error, CustomStringConvertible {
    case missingResource
    case invalidHex(name: String, hex: String)
    case invalidFrameType(name: String, ty: UInt8)

    var description: String {
        switch self {
        case .missingResource:
            return "wire_vectors.json is missing from the test bundle"
        case let .invalidHex(name, hex):
            return "invalid hex for \(name): \(hex)"
        case let .invalidFrameType(name, ty):
            return "invalid frame type \(ty) for vector \(name)"
        }
    }
}

private extension Data {
    init(hex: String, name: String) throws {
        guard hex.count.isMultiple(of: 2) else {
            throw FixtureError.invalidHex(name: name, hex: hex)
        }

        self.init(capacity: hex.count / 2)
        var index = hex.startIndex
        while index < hex.endIndex {
            let next = hex.index(index, offsetBy: 2)
            guard let byte = UInt8(hex[index..<next], radix: 16) else {
                throw FixtureError.invalidHex(name: name, hex: hex)
            }
            self.append(byte)
            index = next
        }
    }

    func lowerHexString() -> String {
        map { String(format: "%02x", $0) }.joined()
    }
}

private func loadWireVectors() throws -> WireVectors {
    let decoder = JSONDecoder()
    decoder.keyDecodingStrategy = .convertFromSnakeCase

    let bundle = Bundle.module
    let url = bundle.url(forResource: "wire_vectors", withExtension: "json", subdirectory: "Fixtures")
        ?? bundle.url(forResource: "wire_vectors", withExtension: "json")
    guard let url else {
        throw FixtureError.missingResource
    }

    let data = try Data(contentsOf: url)
    return try decoder.decode(WireVectors.self, from: data)
}

private func frameType(for vector: FrameVector) throws -> FrameType {
    guard let ty = FrameType(rawValue: vector.ty) else {
        throw FixtureError.invalidFrameType(name: vector.name, ty: vector.ty)
    }
    return ty
}

final class ParityTests: XCTestCase {
    func testComputeProofMatchesRustGoldenVectors() throws {
        let vectors = try loadWireVectors()

        for vector in vectors.proofVectors {
            let actual = computeProof(
                key: try Data(hex: vector.keyHex, name: "\(vector.name).key_hex"),
                domain: vector.domain,
                clientNonce: try Data(hex: vector.clientNonceHex, name: "\(vector.name).client_nonce_hex"),
                serverNonce: try Data(hex: vector.serverNonceHex, name: "\(vector.name).server_nonce_hex"),
                daemonId: try Data(hex: vector.daemonIdHex, name: "\(vector.name).daemon_id_hex")
            )

            XCTAssertEqual(
                actual.lowerHexString(),
                vector.expectedProofHex,
                "Rust proof mismatch for vector \(vector.name)"
            )
        }
    }

    func testEncodeHeaderAndFrameMatchRustGoldenVectors() throws {
        let vectors = try loadWireVectors()

        for vector in vectors.frameVectors {
            let ty = try frameType(for: vector)
            let body = try Data(hex: vector.bodyHex, name: "\(vector.name).body_hex")
            let header = EnvelopeHeader(
                len: UInt32(body.count),
                ver: PROTOCOL_VERSION,
                ty: ty,
                flags: vector.flags,
                channel: vector.channel,
                epoch: vector.epoch,
                corr: vector.corr
            )

            XCTAssertEqual(
                encodeHeader(header).lowerHexString(),
                vector.expectedHeaderHex,
                "Rust header mismatch for vector \(vector.name)"
            )
            XCTAssertEqual(
                try encodeFrame(
                    ty: ty,
                    flags: vector.flags,
                    channel: vector.channel,
                    epoch: vector.epoch,
                    corr: vector.corr,
                    body: body
                ).lowerHexString(),
                vector.expectedFrameHex,
                "Rust frame mismatch for vector \(vector.name)"
            )
        }
    }

    func testDecodeHeaderRoundTripsGoldenHeaders() throws {
        let vectors = try loadWireVectors()

        let old = try XCTUnwrap(vectors.frameVectors.first { $0.name == "error_json_max_epoch" })
        let daemon = try XCTUnwrap(vectors.frameVectors.first { $0.name == "error_json_max_epoch_daemon_origin" })
        let daemonHeader = EnvelopeHeader(
            len: UInt32(try Data(hex: daemon.bodyHex, name: "daemon.body_hex").count),
            ver: PROTOCOL_VERSION,
            ty: try frameType(for: daemon),
            flags: daemon.flags,
            channel: daemon.channel,
            epoch: daemon.epoch,
            corr: daemon.corr
        )
        let oldHeader = EnvelopeHeader(
            len: UInt32(try Data(hex: old.bodyHex, name: "old.body_hex").count),
            ver: PROTOCOL_VERSION,
            ty: try frameType(for: old),
            flags: old.flags,
            channel: old.channel,
            epoch: old.epoch,
            corr: old.corr
        )
        XCTAssertTrue(try decodeHeader(encodeHeader(daemonHeader)).daemonOrigin)
        XCTAssertFalse(try decodeHeader(encodeHeader(oldHeader)).daemonOrigin)

        for vector in vectors.frameVectors {
            let ty = try frameType(for: vector)
            let body = try Data(hex: vector.bodyHex, name: "\(vector.name).body_hex")
            let header = EnvelopeHeader(
                len: UInt32(body.count),
                ver: PROTOCOL_VERSION,
                ty: ty,
                flags: vector.flags,
                channel: vector.channel,
                epoch: vector.epoch,
                corr: vector.corr
            )

            let decoded = try decodeHeader(encodeHeader(header))
            XCTAssertEqual(decoded.len, header.len, "len round-trip mismatch for \(vector.name)")
            XCTAssertEqual(decoded.ver, header.ver, "ver round-trip mismatch for \(vector.name)")
            XCTAssertEqual(decoded.ty, header.ty, "ty round-trip mismatch for \(vector.name)")
            XCTAssertEqual(decoded.flags, header.flags, "flags round-trip mismatch for \(vector.name)")
            XCTAssertEqual(decoded.channel, header.channel, "channel round-trip mismatch for \(vector.name)")
            XCTAssertEqual(decoded.epoch, header.epoch, "epoch round-trip mismatch for \(vector.name)")
            XCTAssertEqual(decoded.corr, header.corr, "corr round-trip mismatch for \(vector.name)")
        }
    }
}
