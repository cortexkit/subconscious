import XCTest
@testable import SubcFed

/// Compile-time and runtime checks that the closed public surface is reachable
/// without importing carrier/session implementation details by name from app code.
final class PublicAPISurfaceTests: XCTestCase {
    func testClosedPublicTypeNamesAreConstructible() throws {
        let _: FedConnectionState = .idle
        let _: FedFailure = .fedEffectsUnsupported
        let _: CandidateFailureReason = .responderKeyMismatch
        let _: CandidateRejectionReason = .addressClassNotAllowed
        let _: CandidateTransportFailureKind = .relayPressure
        let _: FedCandidateStage = .fedNegotiation
        let _: FedAuthenticationKind = .relay
        let _: FedDialPolicy = FedDialPolicy()
        let _: FedJSONValue = .null
        let object = FedJSONObject(["ok": .boolean(true)])
        XCTAssertEqual(object["ok"], .boolean(true))

        let keyStore: any FedPrivateKeyStore = try FedMemoryPrivateKeyStore(
            noisePrivateKey: Data(repeating: 0x42, count: 32)
        )
        let stateStore: any FedStateStore = FedMemoryStateStore()
        XCTAssertNotNil(keyStore)
        XCTAssertNotNil(stateStore)

        let target = try FedManagementTarget(moduleID: "prefrontal-core")
        XCTAssertEqual(target.moduleID, "prefrontal-core")
    }

    func testPackageProductNameIsSubcFed() {
        // The module import above is the product/target name declared in Package.swift.
        XCTAssertEqual(String(reflecting: SubcFedClient.self).contains("SubcFed"), true)
    }
}
