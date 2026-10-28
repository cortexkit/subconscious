import Foundation
import Security
import SubcFed
import XCTest

final class DeviceApprovalSecureEnclaveTestSupportTests: XCTestCase {
    func testKeychainInteractionNotAllowedSkipsKeyCreation() {
        let error = NSError(domain: NSOSStatusErrorDomain, code: -25308)
        XCTAssertThrowsError(try SecureEnclaveTestSupport.skipIfKeychainInteractionNotAllowed(error)) {
            XCTAssertTrue($0 is XCTSkip, "Expected a skip for the observed key-creation error, got \($0)")
        }
    }

    func testOtherNativeErrorsAreRethrownUnchanged() {
        let errors = [
            NSError(domain: "ExampleErrorDomain", code: -25308),
            NSError(domain: NSOSStatusErrorDomain, code: Int(errSecMissingEntitlement)),
            NSError(domain: NSOSStatusErrorDomain, code: Int(errSecAuthFailed)),
            NSError(domain: NSOSStatusErrorDomain, code: Int(errSecParam)),
            NSError(domain: "ExampleErrorDomain", code: 1, userInfo: [
                NSUnderlyingErrorKey: NSError(domain: NSOSStatusErrorDomain, code: -25308),
            ]),
        ]
        for error in errors {
            XCTAssertThrowsError(try SecureEnclaveTestSupport.skipIfKeychainInteractionNotAllowed(error)) {
                XCTAssertTrue(($0 as NSError) === error, "Expected the original error, got \($0)")
            }
        }
    }

    func testProductErrorsAreRethrownUnchanged() {
        let errors = [
            DeviceApprovalError(code: .signingFailed, field: "action_key"),
            DeviceApprovalError(code: .keyRestorationFailed, field: "action_key"),
        ]
        for error in errors {
            XCTAssertThrowsError(try SecureEnclaveTestSupport.skipIfKeychainInteractionNotAllowed(error)) {
                XCTAssertEqual($0 as? DeviceApprovalError, error)
            }
        }
    }
}
