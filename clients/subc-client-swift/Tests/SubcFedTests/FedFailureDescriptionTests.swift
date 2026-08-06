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
}
