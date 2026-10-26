import CryptoKit
import Foundation
import SubcFed
import XCTest

final class DeviceApprovalActionKeyRestorationTests: XCTestCase {
    func testRestoringKeyDataPreservesPublicKeyAndRegistrationRequest() throws {
        #if targetEnvironment(simulator)
        throw XCTSkip("Secure Enclave keys cannot be created on a simulator")
        #else
        guard SecureEnclave.isAvailable else { throw XCTSkip("Secure Enclave unavailable") }
        let original = try DeviceApprovalActionSigner.createKey()
        let restored = try DeviceApprovalActionKey(dataRepresentation: original.dataRepresentation)
        XCTAssertEqual(restored.p256PublicKeyHex, original.p256PublicKeyHex)
        XCTAssertEqual(DeviceApprovalRequests.registrationChallenge(for: restored).body,
                       DeviceApprovalRequests.registrationChallenge(for: original).body)
        #endif
    }

    func testCorruptKeyDataRefusesByName() throws {
        #if targetEnvironment(simulator)
        let expected = DeviceApprovalError(code: .secureEnclaveUnavailable, field: "secure_enclave")
        #else
        let expected = SecureEnclave.isAvailable
            ? DeviceApprovalError(code: .keyRestorationFailed, field: "action_key")
            : DeviceApprovalError(code: .secureEnclaveUnavailable, field: "secure_enclave")
        #endif
        for blob in [Data(), Data([0, 1, 2, 255]), Data(repeating: 0, count: 32)] {
            XCTAssertThrowsError(try DeviceApprovalActionKey(dataRepresentation: blob)) {
                XCTAssertEqual($0 as? DeviceApprovalError, expected)
            }
        }
    }
}
