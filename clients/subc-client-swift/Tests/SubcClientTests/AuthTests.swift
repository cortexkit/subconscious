import Foundation
import XCTest
@testable import SubcClient

private enum ScriptedAuthTransportError: Error {
    case inputExhausted
}

private final class ScriptedAuthTransport: Transport {
    private let input: Data
    private var offset = 0

    init(message: [String: Any]) throws {
        let body = try JSONSerialization.data(withJSONObject: message)
        var prefix = Data(count: 4)
        let length = UInt32(body.count).littleEndian
        withUnsafeBytes(of: length) { prefix.replaceSubrange(0..<4, with: $0) }
        var input = prefix
        input.append(body)
        self.input = input
    }

    func writeAll(_: Data) throws {}

    func readExact(_ count: Int) throws -> Data {
        let end = offset + count
        guard end <= input.count else { throw ScriptedAuthTransportError.inputExhausted }
        defer { offset = end }
        return input.subdata(in: offset..<end)
    }

    func close() {}
}

final class AuthTests: XCTestCase {
    func testRandomNonceProducesDistinctNonzeroValuesOfRequestedLength() throws {
        let nonces = try (0..<4).map { _ in try randomNonce(NONCE_LEN) }

        XCTAssertTrue(nonces.allSatisfy { $0.count == NONCE_LEN })
        XCTAssertTrue(nonces.allSatisfy { $0.contains(where: { $0 != 0 }) })
        XCTAssertEqual(Set(nonces).count, nonces.count)
    }

    func testRejectsOutOfRangeProofByteBeforeHMACVerification() throws {
        var serverProof = Array(repeating: 0, count: PROOF_LEN)
        serverProof[0] = 427
        let transport = try ScriptedAuthTransport(message: [
            "daemon_id": Array(repeating: 3, count: DAEMON_ID_LEN),
            "server_nonce": Array(repeating: 2, count: NONCE_LEN),
            "daemon_ver": "test",
            "server_proof": serverProof,
        ])
        let connection = ConnectionInfo(
            schema: SCHEMA_VERSION,
            endpoints: [Endpoint(host: "127.0.0.1", port: 8799)],
            key: Data(repeating: 0xAB, count: MIN_KEY_LEN),
            daemonId: Data(repeating: 3, count: DAEMON_ID_LEN),
            pid: 1,
            daemonVer: "test"
        )

        XCTAssertThrowsError(try authenticateClient(transport, connection)) { error in
            guard let authError = error as? AuthError else {
                return XCTFail("expected AuthError, got \(error)")
            }
            XCTAssertTrue(
                authError.message.contains("auth field 'server_proof' has invalid byte 427"),
                "unexpected auth error: \(authError.message)"
            )
        }
    }
}
