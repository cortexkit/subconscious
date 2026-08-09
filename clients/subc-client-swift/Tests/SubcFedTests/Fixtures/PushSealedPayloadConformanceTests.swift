import Foundation
import XCTest
@testable import SubcFed

/// Conformance harness for the sealed push payload corpus
/// (`docs/specs/push-sealed-payload.md`).
///
/// The corpus is minted by the Rust sealer in `callosum` and consumed here, so
/// the cross-implementation property is tested by a side that had no hand in
/// producing the bytes. A round-trip test would be satisfied by two
/// implementations that are each internally consistent and disagree with each
/// other, which is the failure a shared corpus exists to catch.
///
/// THIS LANDS BEFORE THE CORPUS DOES, DELIBERATELY. The usual failure is that a
/// harness arrives after the data and nobody ever checks it can distinguish an
/// empty corpus from a passing one -- so it is written here against an absent
/// corpus first, where that distinction is the only thing it can be tested on.
final class PushSealedPayloadConformanceTests: XCTestCase {

    /// Where the corpus lands when `callosum`'s sealer emits it. Resolved the
    /// same way the rdv-wire fixtures are, so both obey `SUBC_FED_PACKAGE_PATH`.
    private static var corpusDirectory: URL {
        let packageRoot = ProcessInfo.processInfo.environment["SUBC_FED_PACKAGE_PATH"]
            ?? FileManager.default.currentDirectoryPath
        return URL(fileURLWithPath: packageRoot)
            .appendingPathComponent("Tests/SubcFedTests/Fixtures/push-sealed")
    }

    private static var corpusFile: URL {
        corpusDirectory.appendingPathComponent("vectors.jsonl")
    }

    /// AN ABSENT CORPUS AND AN EMPTY ONE ARE DIFFERENT STATES AND MUST NOT READ
    /// ALIKE.
    ///
    /// Absent is the expected pre-delivery state and skips with the path named,
    /// so the skip says what has to appear rather than merely that something did
    /// not run. PRESENT-BUT-EMPTY IS A FAILURE: that is the shape where a
    /// generator ran, produced nothing, and every table below would pass over
    /// zero rows -- green because it examined nothing.
    private func loadVectors() throws -> [[String: Any]] {
        let directoryExists = FileManager.default.fileExists(atPath: Self.corpusDirectory.path)
        guard directoryExists else {
            throw XCTSkip(
                """
                push-sealed corpus not delivered yet; expected at \
                \(Self.corpusFile.path). Minted by callosum's sealer per \
                docs/specs/push-sealed-payload.md. This suite enforces the moment \
                the file appears.
                """
            )
        }

        let content = try String(contentsOf: Self.corpusFile, encoding: .utf8)
        let rows = try content
            .split(separator: "\n")
            .filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
            .map { line -> [String: Any] in
                guard let object = try JSONSerialization.jsonObject(
                    with: Data(line.utf8)
                ) as? [String: Any] else {
                    throw VectorError.notAnObject(String(line.prefix(80)))
                }
                return object
            }

        XCTAssertFalse(
            rows.isEmpty,
            "corpus file exists at \(Self.corpusFile.path) and contains no vectors -- "
                + "a generator that ran and produced nothing, not a clean pass"
        )
        return rows
    }

    private enum VectorError: Error {
        case notAnObject(String)
    }

    /// The corpus must contain every negative the spec names, each of which is a
    /// distinct typed refusal on the opener.
    ///
    /// Checked as a SET DIFFERENCE against the spec's list rather than by
    /// counting: a count agrees when one required vector is missing and an
    /// unrelated one was added, and the added one is not evidence about the
    /// missing one.
    func testCorpusCarriesEveryNegativeTheSpecRequires() throws {
        let vectors = try loadVectors()
        let required: Set<String> = [
            "unknown_version",
            "truncated_enc",
            "truncated_ct",
            "tampered_ct",
            "tampered_enc",
            "wrong_recipient",
            "empty_ct",
        ]
        let present = Set(vectors.compactMap { $0["name"] as? String })
        let missing = required.subtracting(present)
        XCTAssertTrue(
            missing.isEmpty,
            "corpus is missing required negative vectors: \(missing.sorted())"
        )

        // A negative-only corpus cannot distinguish a correct opener from one
        // that refuses everything -- so the positive controls are required in the
        // same breath as the negatives they guard.
        XCTAssertTrue(
            present.contains(where: { $0.hasPrefix("valid_") }),
            "corpus carries no valid vector; every refusal below would be "
                + "satisfied by an opener that refuses unconditionally"
        )
    }

    /// `wrong_recipient` is the vector that turns a rule into a test: it MUST be
    /// sealed to a different DEDICATED key, so an implementation that resolved
    /// the recipient to the device's Noise transport key fails it.
    ///
    /// Asserted on the corpus itself rather than only on opener behaviour,
    /// because a vector generated with the wrong key would pass the refusal test
    /// for the wrong reason and quietly stop testing the substitution.
    func testWrongRecipientVectorIsSealedToADistinctKey() throws {
        let vectors = try loadVectors()
        guard let vector = vectors.first(where: { $0["name"] as? String == "wrong_recipient" })
        else {
            return XCTFail("corpus is missing the wrong_recipient vector")
        }

        let recipient = vector["recipient_public_key_hex"] as? String
        let sealedTo = vector["sealed_to_public_key_hex"] as? String
        XCTAssertNotNil(recipient, "vector must name the key the opener will use")
        XCTAssertNotNil(sealedTo, "vector must name the key it was actually sealed to")
        XCTAssertNotEqual(
            recipient, sealedTo,
            "wrong_recipient must be sealed to a DIFFERENT key, or it tests nothing"
        )
    }

    /// Every vector must be self-describing: the spec's envelope is
    /// `version ‖ enc ‖ ct`, and the version byte is checked FIRST so a future
    /// authenticated mode is a bump rather than a silent reinterpretation.
    func testEveryVectorCarriesTheFieldsTheSpecRequires() throws {
        let vectors = try loadVectors()
        for vector in vectors {
            let name = vector["name"] as? String ?? "<unnamed>"
            XCTAssertNotNil(vector["sealed_base64"], "\(name): missing sealed_base64")
            XCTAssertNotNil(
                vector["recipient_private_key_hex"],
                "\(name): missing recipient private key -- the opener cannot run"
            )
            if name.hasPrefix("valid_") {
                XCTAssertNotNil(
                    vector["plaintext_utf8"],
                    "\(name): a valid vector must state its expected plaintext"
                )
                // Present so sealing is checkable as DETERMINISTIC rather than
                // merely reversible. Nothing needs it to decrypt, which is why
                // the spec marks it MUST NOT remove.
                XCTAssertNotNil(
                    vector["ephemeral_private_key_hex"],
                    "\(name): missing ephemeral key -- without it the only "
                        + "checkable property is that each side opens its own output"
                )
            }
        }
    }
}
