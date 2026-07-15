import Foundation
import XCTest
@testable import SubcClient

final class AuthTests: XCTestCase {
    func testRandomNonceProducesDistinctNonzeroValuesOfRequestedLength() throws {
        let nonces = try (0..<4).map { _ in try randomNonce(NONCE_LEN) }

        XCTAssertTrue(nonces.allSatisfy { $0.count == NONCE_LEN })
        XCTAssertTrue(nonces.allSatisfy { $0.contains(where: { $0 != 0 }) })
        XCTAssertEqual(Set(nonces).count, nonces.count)
    }
}
