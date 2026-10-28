import CryptoKit
import Foundation
import Security
import XCTest

enum SecureEnclaveTestSupport {
    /// A test process may not create a Secure Enclave key with user-presence access control
    /// when keychain interaction is not allowed (errSecInteractionNotAllowed), even if the
    /// enclave is available. Probe the native API because the product API discards its underlying
    /// error. Keep product key creation and restoration outside this probe so their failures still
    /// fail the test.
    static func requireKeyCreation() throws {
        #if targetEnvironment(simulator)
        throw XCTSkip("Secure Enclave keys cannot be created on a simulator")
        #else
        guard SecureEnclave.isAvailable else { throw XCTSkip("Secure Enclave unavailable") }
        do {
            var error: Unmanaged<CFError>?
            let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                                                        [.privateKeyUsage, .userPresence], &error)
            if let error { throw error.takeRetainedValue() }
            let accessControl = try XCTUnwrap(access, "Security did not return access control")
            _ = try SecureEnclave.P256.Signing.PrivateKey(accessControl: accessControl)
        } catch {
            try skipIfKeychainInteractionNotAllowed(error)
        }
        #endif
    }

    static func skipIfKeychainInteractionNotAllowed(_ error: Error) throws {
        let nativeError = error as NSError
        if nativeError.domain == NSOSStatusErrorDomain,
           nativeError.code == Int(errSecInteractionNotAllowed) {
            throw XCTSkip("This test process cannot create a Secure Enclave key with user-presence access control: keychain interaction not allowed (\(nativeError.domain), \(nativeError.code))")
        }
        throw error
    }
}
