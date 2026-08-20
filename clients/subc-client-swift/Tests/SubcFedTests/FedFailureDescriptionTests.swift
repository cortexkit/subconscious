import XCTest
@testable import SubcFed

/// The point of the conformance is that a failure reaches a person as a
/// sentence rather than as a dump of the enum's structure. These assert that
/// property directly, because it is the one that breaks silently: adding a case
/// without a description still compiles, and the dumped form still "works".
final class FedFailureDescriptionTests: XCTestCase {
    /// The exact shape reported from the phone: a correct explanation arriving
    /// wrapped in the syntax used to print it.
    func testModuleErrorMessageIsUsedVerbatimWithoutOptionalWrapper() {
        let failure = FedFailure.moduleError(
            code: "unknown_member",
            message: "ck-app:someone is not a member of rm_abc"
        )

        XCTAssertEqual(failure.description, "ck-app:someone is not a member of rm_abc")
        XCTAssertFalse(failure.description.contains("Optional("))
        XCTAssertFalse(failure.description.contains("moduleError("))
    }

    /// A module may send a code alone, and the fallback still has to read as a
    /// sentence rather than as a bare token.
    func testModuleErrorWithoutMessageFallsBackToTheCode() {
        let failure = FedFailure.moduleError(code: "unknown_member", message: nil)

        XCTAssertTrue(failure.description.contains("unknown_member"))
        XCTAssertFalse(failure.description.contains("Optional"))
        XCTAssertTrue(failure.description.hasSuffix("."))
    }

    /// Every case needs prose, not only the ones somebody happened to hit. A
    /// case with no description falls back to the enum's structural dump, so
    /// this checks that the enum's own syntax never appears in the output.
    func testNoDescriptionRendersAsAStructuralDump() {
        let failures: [FedFailure] = [
            .notDialOwner,
            .unsupportedEnrollmentClass,
            .storeLossReenrollmentRequired,
            .invalidProfile(field: "control_url"),
            .candidateTimedOut(stage: .carrierConnect),
            .relayAuthenticationFailed(code: "expired"),
            .responderKeyMismatch,
            .accountKeyMismatch,
            .noiseAuthenticationFailed,
            .framingViolation,
            .protocolViolation(byeCode: "bad_frame"),
            .catalogTargetUnavailable,
            .fedBodyTooLarge,
            .fedEffectsUnsupported,
            .storeCorrupt,
            .storeUnavailable,
            .storeMigrationFailed,
            .reservationFailed,
            .persistenceFailed,
            .cancelled,
            .suspended,
            .disconnected,
            .moduleError(code: "x", message: "y"),
            .indeterminateMutation,
            .admissionQueueFull,
            .admissionQueueTimedOut,
            .noEligibleCandidates([]),
            .allCandidatesFailed([]),
        ]

        for failure in failures {
            let text = failure.description
            XCTAssertFalse(text.isEmpty, "empty description")
            XCTAssertFalse(text.contains("Optional("), "leaked wrapper: \(text)")
            // A structural dump names the case and opens a parenthesis straight
            // after it; prose never does.
            XCTAssertFalse(
                text.hasPrefix("invalidProfile(") || text.hasPrefix("moduleError(")
                    || text.hasPrefix("candidateTimedOut("),
                "structural dump: \(text)"
            )
        }
    }

    func testStoreLossReenrollmentFailureRoundTripsAndNamesTheCeremony() throws {
        let failure = FedFailure.storeLossReenrollmentRequired
        let encoded = try JSONEncoder().encode(failure)
        XCTAssertEqual(try JSONDecoder().decode(FedFailure.self, from: encoded), failure)

        let text = failure.description
        XCTAssertTrue(text.contains("state was reset"), "lost the store-loss condition: \(text)")
        XCTAssertTrue(text.contains("re-enrollment"), "lost the operator remedy: \(text)")
        XCTAssertTrue(text.contains("rollback replay"), "lost the serving-side reason: \(text)")
        XCTAssertNotEqual(text, FedFailure.storeCorrupt.description)
        XCTAssertNotEqual(text, FedFailure.storeUnavailable.description)
    }

    /// The two authority bye codes have OPPOSITE subjects: `fed_tombstoned`
    /// means THIS device was revoked; `fed_local_membership_fenced` means the
    /// SENDER fenced itself and this device is healthy. The strings are pinned
    /// by the producer (callosum test-vectors/fed-wire, vendored in
    /// Fixtures/fed-terminal-bye-codes.jsonl); these tests assert each renders
    /// its own subject and can never borrow the other's text — rendering
    /// "revoked" for a self-fenced sender sends an operator to re-enroll a
    /// device that has no problem.
    func testTombstonedByeRendersTheRecipientAsRevoked() throws {
        let codes = try Self.vendoredByeCodes()
        let code = try XCTUnwrap(codes["bye_recipient_tombstoned"])
        let text = FedFailure.protocolViolation(byeCode: code).description
        XCTAssertTrue(text.contains("revoked"), "lost the revocation subject: \(text)")
        XCTAssertTrue(text.contains("re-enroll"), "lost the remedy: \(text)")
        XCTAssertFalse(text.contains("No action"), "borrowed the fenced-sender text: \(text)")
        XCTAssertFalse(text.contains("protocol error"), "fell through to the generic arm: \(text)")
    }

    func testMembershipFencedByeRendersTheSenderAsTheSubject() throws {
        let codes = try Self.vendoredByeCodes()
        let code = try XCTUnwrap(codes["bye_sender_membership_fenced"])
        let text = FedFailure.protocolViolation(byeCode: code).description
        XCTAssertTrue(text.contains("its own account membership"), "lost the sender subject: \(text)")
        XCTAssertTrue(text.contains("No action is needed on this device"), "lost the reassurance: \(text)")
        XCTAssertFalse(text.contains("revoked by the remote peer"), "borrowed the tombstoned text \u{2014} the inversion this split exists to prevent: \(text)")
        XCTAssertFalse(text.contains("protocol error"), "fell through to the generic arm: \(text)")
    }

    /// Unknown bye codes must keep the generic terminal rendering — the
    /// tolerant-classification property that makes new producer codes
    /// transport-safe before a rendering ships.
    func testUnknownByeCodeKeepsTheGenericTerminalRendering() {
        let text = FedFailure.protocolViolation(byeCode: "fed_some_future_code").description
        XCTAssertTrue(text.contains("fed_some_future_code"), "lost the code: \(text)")
        XCTAssertTrue(text.contains("protocol error"), "generic arm changed shape: \(text)")
    }

    /// Reads the code strings from the vendored producer-minted fixture rather
    /// than repeating the literals: if callosum renames a code, the fixture
    /// refresh changes these tests' inputs and the rendering switch goes red
    /// here instead of silently falling through to the generic arm in
    /// production.
    private static func vendoredByeCodes() throws -> [String: String] {
        // Source-tree resolution, matching the target's other fixture readers
        // (this test target declares no bundle resources).
        let packageRoot = ProcessInfo.processInfo.environment["SUBC_FED_PACKAGE_PATH"]
            ?? FileManager.default.currentDirectoryPath
        let url = URL(fileURLWithPath: packageRoot)
            .appendingPathComponent("Tests/SubcFedTests/Fixtures/fed-terminal-bye-codes.jsonl")
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "vendored fixture missing: \(url.path)"
        )
        let lines = try String(contentsOf: url, encoding: .utf8)
            .split(separator: "\n").filter { !$0.isEmpty }
        XCTAssertEqual(lines.count, 2, "fixture must carry exactly the two split codes")
        var codes: [String: String] = [:]
        for line in lines {
            let obj = try XCTUnwrap(
                try JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any])
            let name = try XCTUnwrap(obj["name"] as? String)
            let header = try XCTUnwrap(obj["header"] as? [String: Any])
            codes[name] = try XCTUnwrap(header["code"] as? String)
        }
        return codes
    }
}
