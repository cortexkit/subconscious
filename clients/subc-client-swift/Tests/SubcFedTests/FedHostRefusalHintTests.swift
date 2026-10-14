import Foundation
import XCTest
@testable import SubcFed

/// The host-refusal hint is a CLOSED vocabulary read from a
/// `fed_target_unavailable` error body. These tests pin every way the decoder
/// must decline to produce a hint (or a reason) rather than pass an unknown
/// value through or fail the call.
final class FedHostRefusalHintTests: XCTestCase {
    func testScopeBindProofCodesUseTheTerminalModuleErrorPath() throws {
        for code in [ScopeBindProofCodes.required, ScopeBindProofCodes.mismatch] {
            let frame = errorFrame(#"{"code":"\#(code)"}"#)
            XCTAssertEqual(frame.terminalCode, code)
            XCTAssertNil(frame.terminalHostRefusal)
            let failure = FedFailure.moduleError(code: code)
            XCTAssertEqual(try JSONDecoder().decode(FedFailure.self, from: JSONEncoder().encode(failure)), failure)
            XCTAssertFalse(failure.description.isEmpty)
            XCTAssertNotNil(ScopeBindProofCodes.message(for: code))
        }
    }

    private func errorFrame(_ json: String) -> FedFrame {
        FedFrame(
            type: "call_frame",
            fields: ["k": .string("error"), "last": .bool(true)],
            body: Data(json.utf8)
        )
    }

    func testUnknownHostRefusalYieldsNoHint() {
        let frame = errorFrame(#"""
        {"code":"fed_target_unavailable","detail":{"host_refusal":"module_exploded"}}
        """#)
        XCTAssertEqual(frame.terminalCode, "fed_target_unavailable")
        XCTAssertNil(frame.terminalHostRefusal?.hostRefusal)
    }

    func testHostReasonIsIgnoredWhenRefusalIsNotWarming() {
        let frame = errorFrame(#"""
        {"code":"fed_target_unavailable",
         "detail":{"host_refusal":"module_reloading","host_reason":"declared_not_ready"}}
        """#)
        XCTAssertEqual(frame.terminalHostRefusal?.hostRefusal, .moduleReloading)
        XCTAssertNil(frame.terminalHostRefusal?.hostReason)
    }

    func testUnknownHostReasonIsDroppedButRefusalKept() {
        let frame = errorFrame(#"""
        {"code":"fed_target_unavailable",
         "detail":{"host_refusal":"module_warming","host_reason":"moon_phase","extra":1}}
        """#)
        XCTAssertEqual(frame.terminalHostRefusal?.hostRefusal, .moduleWarming)
        XCTAssertNil(frame.terminalHostRefusal?.hostReason)
    }

    func testValidDetailOnADifferentCodeYieldsNoHint() {
        let frame = errorFrame(#"""
        {"code":"fed_not_exposed",
         "detail":{"host_refusal":"module_warming","host_reason":"declared_not_ready"}}
        """#)
        XCTAssertNil(frame.terminalHostRefusal?.hostRefusal)
    }

    func testDetailAsStringYieldsNoHint() {
        let frame = errorFrame(#"""
        {"code":"fed_target_unavailable","detail":"module_warming"}
        """#)
        XCTAssertNil(frame.terminalHostRefusal?.hostRefusal)
    }

    func testMissingDetailYieldsNoHint() {
        let frame = errorFrame(#"{"code":"fed_target_unavailable","message":"gone"}"#)
        XCTAssertEqual(frame.terminalMessage, "gone")
        XCTAssertNil(frame.terminalHostRefusal?.hostRefusal)
    }

    func testEveryKnownHostRefusalDecodes() {
        for refusal in FedHostRefusalHint.HostRefusal.allCases {
            let frame = errorFrame(
                #"{"code":"fed_target_unavailable","detail":{"host_refusal":"\#(refusal.rawValue)"}}"#
            )
            XCTAssertEqual(frame.terminalHostRefusal, FedHostRefusalHint(hostRefusal: refusal))
        }
    }

    // MARK: - FedFailure Codable

    func testModuleErrorRoundTripsWithAndWithoutHint() throws {
        let failures: [FedFailure] = [
            .moduleError(code: "fed_target_unavailable", message: "warming"),
            .moduleError(
                code: "fed_target_unavailable",
                message: nil,
                hostRefusal: FedHostRefusalHint(hostRefusal: .moduleWarming, hostReason: .declaredNotReady)
            ),
            .moduleError(
                code: "fed_target_unavailable",
                hostRefusal: FedHostRefusalHint(hostRefusal: .moduleTimeout)
            ),
        ]
        for failure in failures {
            let data = try JSONEncoder().encode(failure)
            XCTAssertEqual(try JSONDecoder().decode(FedFailure.self, from: data), failure)
        }
    }

    /// A failure persisted by a build that predates the hint has no hint key and
    /// must still decode, with the hint absent.
    func testModuleErrorWithoutHintKeyDecodes() throws {
        let legacy = Data(#"{"kind":"moduleError","code":"not_a_member","message":"nope"}"#.utf8)
        let decoded = try JSONDecoder().decode(FedFailure.self, from: legacy)
        XCTAssertEqual(decoded, .moduleError(code: "not_a_member", message: "nope", hostRefusal: nil))
    }
}
