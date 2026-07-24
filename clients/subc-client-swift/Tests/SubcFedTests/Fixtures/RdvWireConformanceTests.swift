import Foundation
import XCTest
@testable import SubcFed

/// Byte-exact rdv-wire canonical-form conformance against the vendored golden
/// vectors (docs/rdv-wire.md §1.2). Every vector file is consumed non-vacuously:
/// canonical-valid must canonicalize byte-for-byte, parse-reject must ALL reject,
/// decimal-string and nesting-depth boundaries must hold, and the shared
/// candidate/registry-row fixture must decode and re-canonicalize exactly.
final class RdvWireConformanceTests: XCTestCase {

    // MARK: - canonical-valid.jsonl

    func testCanonicalValidVectorsCanonicalizeByteExact() throws {
        let lines = try RdvWireFixtures.jsonlLines("canonical-valid.jsonl")
        XCTAssertGreaterThan(lines.count, 0)

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let value = try XCTUnwrap(entry["value"])
            let canonical = try XCTUnwrap(entry["canonical"] as? String)

            // Feeding `value` (any key order) through the canonicalizer must
            // produce exactly the `canonical` string, byte for byte.
            let rdvValue = try RdvJSONValue(any: value)
            let produced = try RdvCanonicalJSON.canonicalString(rdvValue)
            XCTAssertEqual(produced, canonical, "canonical mismatch for vector \(name)")
            XCTAssertEqual(Data(produced.utf8), Data(canonical.utf8), "canonical bytes mismatch for \(name)")
        }
    }

    func testCanonicalValidVectorsRoundTripThroughStrictParser() throws {
        let lines = try RdvWireFixtures.jsonlLines("canonical-valid.jsonl")

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let canonical = try XCTUnwrap(entry["canonical"] as? String)

            // The canonical form is valid rdv-wire JSON: the strict parser accepts
            // it, and re-canonicalizing the parse is a fixed point (byte-identical).
            let parsed = try RdvJSONValue.parse(Data(canonical.utf8))
            let recanonicalized = try RdvCanonicalJSON.canonicalString(parsed)
            XCTAssertEqual(recanonicalized, canonical, "round-trip not stable for vector \(name)")
        }
    }

    // MARK: - parse-reject.jsonl

    func testParseRejectVectorsAllReject() throws {
        let lines = try RdvWireFixtures.jsonlLines("parse-reject.jsonl")
        XCTAssertGreaterThan(lines.count, 0)

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let raw = try XCTUnwrap(entry["raw"] as? String)
            let reason = try XCTUnwrap(entry["reason"] as? String)

            // Every reject vector must be rejected by the strict rdv-wire parser —
            // no carve-outs (number literals, non-NFC, non-minimal escapes, and
            // duplicate keys are all outside the rdv-wire domain).
            XCTAssertThrowsError(
                try RdvJSONValue.parse(Data(raw.utf8)),
                "vector \(name) (\(reason)) must be rejected"
            )
        }
    }

    // MARK: - decimal-string.jsonl

    func testDecimalStringDiscipline() throws {
        let lines = try RdvWireFixtures.jsonlLines("decimal-string.jsonl")
        XCTAssertGreaterThan(lines.count, 0)

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let value = try XCTUnwrap(entry["value"] as? String)
            let isValid = try XCTUnwrap(entry["valid"] as? Bool)

            XCTAssertEqual(RdvDecimalString.isValid(value), isValid, "decimal-string \(name)")
        }
    }

    // MARK: - nesting-depth.jsonl

    func testNestingDepthBoundaryAt128Containers() throws {
        let lines = try RdvWireFixtures.jsonlLines("nesting-depth.jsonl")
        XCTAssertGreaterThan(lines.count, 0)

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let arrayDepth = try XCTUnwrap(entry["array_depth"] as? Int)
            let isValid = try XCTUnwrap(entry["valid"] as? Bool)

            // Root object plus `array_depth` arrays around a string leaf. 128 total
            // containers is accepted; the 129th is rejected.
            var json = "{\"leaf\":"
            for _ in 0..<arrayDepth { json += "[" }
            json += "\"x\""
            for _ in 0..<arrayDepth { json += "]" }
            json += "}"

            do {
                _ = try RdvJSONValue.parse(Data(json.utf8))
                XCTAssertTrue(isValid, "vector \(name) should be rejected but parsed")
            } catch let error as RdvJSONError {
                XCTAssertFalse(isValid, "vector \(name) should parse but threw \(error)")
                guard case .nestingTooDeep = error else {
                    XCTFail("vector \(name) rejected for the wrong reason: \(error)")
                    return
                }
            }
        }
    }

    // MARK: - candidate-record.jsonl

    func testCandidateRecordDecodesAndCanonicalizesByteExact() throws {
        let lines = try RdvWireFixtures.jsonlLines("candidate-record.jsonl")
        XCTAssertGreaterThan(lines.count, 0)

        for line in lines {
            let entry = try XCTUnwrap(try JSONSerialization.jsonObject(with: line) as? [String: Any])
            let name = try XCTUnwrap(entry["name"] as? String)
            let value = try XCTUnwrap(entry["value"])
            let canonical = try XCTUnwrap(entry["canonical"] as? String)
            let expectedDialOrder = try XCTUnwrap(entry["expected_public_dial_order"] as? [String])

            // The registry row decodes under deny-unknown-fields.
            let rdvValue = try RdvJSONValue(any: value)
            guard case .object(let object) = rdvValue else {
                XCTFail("candidate-record \(name) is not an object")
                return
            }
            let row = try RdvRegistryRow.decode(object)

            // Mandatory per-candidate provenance is present on every candidate.
            for candidate in row.candidates {
                XCTAssertFalse(candidate.provenance.rawValue.isEmpty, "candidate \(name) missing provenance")
            }

            // Re-canonicalizing the row reproduces the canonical bytes exactly
            // (relay candidate carries no addr; keys sorted at every depth).
            let produced = try RdvCanonicalJSON.canonicalString(.object(object))
            XCTAssertEqual(produced, canonical, "canonical mismatch for candidate-record \(name)")

            // Public dial order is observed-before-self_reported.
            let dialOrder = row.publicDialOrder.compactMap { $0.addr }
            XCTAssertEqual(dialOrder, expectedDialOrder, "public dial order mismatch for \(name)")
        }
    }
}
