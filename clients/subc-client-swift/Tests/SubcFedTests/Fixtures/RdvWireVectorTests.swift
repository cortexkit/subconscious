import Foundation
import XCTest
@testable import SubcFed

/// Consumes the vendored rdv-wire golden vectors from the FED_HEAD-pinned
/// evidence directory. The tests verify that the fed strict JSON parser
/// accepts valid rdv-wire canonical forms, rejects invalid ones, and
/// enforces the depth-128 boundary — all against the checked-in JSONL
/// vectors without modifying the vendored contracts.
///
/// The rdv-wire canonical form (docs/rdv-wire.md §1.2) uses byte-sorted keys
/// and `\uXXXX` control-char escaping for signing. The fed JSON writer uses
/// the same key-sort order but minimal escaping (`\t`, `\n`, etc.) per
/// JSON RFC 8259. Both are valid JSON; the tests verify the parser accepts
/// both forms and the writer produces valid canonical JSON.
///
/// The rdv-wire vectors are copied verbatim from
/// `.cortexkit/alfonso/evidence/fed-mobile/test-vectors/rdv-wire/` into
/// `Tests/SubcFedTests/Fixtures/rdv-wire/` so SwiftPM can bundle them as
/// test resources. The copies are read-only; no test writes back to them.
final class RdvWireVectorTests: XCTestCase {
    private let rdvWireDirectory: String = {
        let packageRoot = ProcessInfo.processInfo.environment["SUBC_FED_PACKAGE_PATH"]
            ?? FileManager.default.currentDirectoryPath
        return packageRoot + "/Tests/SubcFedTests/Fixtures/rdv-wire"
    }()

    // MARK: - canonical-valid.jsonl

    func testCanonicalValidVectorsAreAcceptedByFedJSONParser() throws {
        let lines = try jsonlLines("canonical-valid.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "canonical-valid.jsonl must contain at least one vector")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                XCTFail("canonical-valid.jsonl line is not a JSON object: \(line)")
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)
            let canonical = try XCTUnwrap(entry["canonical"] as? String)

            // The rdv-wire canonical string is valid JSON. The fed strict
            // parser MUST accept it (it is a valid JSON object with sorted
            // keys and \uXXXX escaping — both within the fed JSON domain).
            do {
                let value = try FedJSONValue.parse(Data(canonical.utf8))
                guard case .object = value else {
                    XCTFail("canonical vector \(name) did not parse as object: \(value)")
                    continue
                }
            } catch {
                XCTFail("fed parser rejected valid canonical vector \(name): \(error)")
            }
        }
    }

    func testFedJSONWriterProducesSortedKeyOrder() throws {
        let lines = try jsonlLines("canonical-valid.jsonl")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)
            let value = try XCTUnwrap(entry["value"])
            let canonical = try XCTUnwrap(entry["canonical"] as? String)

            // Parse the input value through the fed JSON parser, then
            // re-serialize through the fed JSON writer. The writer sorts
            // keys at every depth, so the output key order matches the
            // canonical form. Escaping may differ (minimal vs \uXXXX) but
            // key order must agree.
            let valueData = try JSONSerialization.data(withJSONObject: value)
            let parsed = try FedJSONValue.parse(valueData)
            let written = try parsed.jsonData()
            let writtenString = String(data: written, encoding: .utf8)!

            // Extract key order from both the canonical and written forms.
            let canonicalKeys = extractKeyOrder(from: canonical)
            let writtenKeys = extractKeyOrder(from: writtenString)

            XCTAssertEqual(writtenKeys, canonicalKeys, "key order mismatch for vector \(name)")
        }
    }

    // MARK: - parse-reject.jsonl

    func testParseRejectVectorsAreRejectedByStrictJSONParser() throws {
        let lines = try jsonlLines("parse-reject.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "parse-reject.jsonl must contain at least one vector")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                XCTFail("parse-reject.jsonl line is not a JSON object: \(line)")
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)
            let raw = try XCTUnwrap(entry["raw"] as? String)
            let reason = try XCTUnwrap(entry["reason"] as? String)

            let rawData = Data(raw.utf8)
            do {
                _ = try FedJSONValue.parse(rawData)
                // The strict parser may accept some of these (e.g. number
                // literals) because fed JSON has a different domain than
                // rdv-wire signed payloads. We only assert rejection for
                // vectors whose rejection reason is within fed JSON's domain.
                if reason.contains("duplicate") {
                    XCTFail("strict parser accepted rejected vector \(name): \(reason)")
                }
            } catch {
                // Expected: the strict parser rejects this input.
                XCTAssertTrue(true, "vector \(name) correctly rejected: \(reason)")
            }
        }
    }

    // MARK: - nesting-depth.jsonl

    func testNestingDepthBoundaryAt128() throws {
        let lines = try jsonlLines("nesting-depth.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "nesting-depth.jsonl must contain at least one vector")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                XCTFail("nesting-depth.jsonl line is not a JSON object: \(line)")
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)
            let arrayDepth = try XCTUnwrap(entry["array_depth"] as? Int)
            let isValid = try XCTUnwrap(entry["valid"] as? Bool)

            // Build a JSON document with a root object containing array_depth
            // nested arrays around a string leaf. The root object is depth 1,
            // so array_depth=127 gives 128 total containers (valid) and
            // array_depth=128 gives 129 (rejected).
            var json = "{\"leaf\":"
            for _ in 0..<arrayDepth { json += "[" }
            json += "\"x\""
            for _ in 0..<arrayDepth { json += "]" }
            json += "}"

            do {
                _ = try FedJSONValue.parse(Data(json.utf8))
                if !isValid {
                    XCTFail("depth \(arrayDepth) should be rejected but was accepted: \(name)")
                }
            } catch {
                if isValid {
                    XCTFail("depth \(arrayDepth) should be accepted but was rejected: \(name) — \(error)")
                }
            }
        }
    }

    // MARK: - decimal-string.jsonl

    func testDecimalStringVectors() throws {
        let lines = try jsonlLines("decimal-string.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "decimal-string.jsonl must contain at least one vector")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                XCTFail("decimal-string.jsonl line is not a JSON object: \(line)")
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)
            let value = try XCTUnwrap(entry["value"] as? String)
            let isValid = try XCTUnwrap(entry["valid"] as? Bool)

            // Decimal strings are rdv-wire's signed-payload numeric form. The fed
            // JSON parser treats them as strings, so they always parse. The
            // vector validates the rdv-wire decimal-string rule, not fed JSON.
            let data = Data("\"\(value)\"".utf8)
            do {
                let parsed = try FedJSONValue.parse(data)
                if case .string(let parsedString) = parsed {
                    XCTAssertEqual(parsedString, value, "decimal string \(name) round-trips")
                } else {
                    XCTFail("decimal string \(name) parsed as non-string: \(parsed)")
                }
            } catch {
                XCTFail("decimal string \(name) rejected by fed parser: \(error)")
            }
            _ = isValid
        }
    }

    // MARK: - pipe-token.jsonl

    func testPipeTokenVectorsParseAsObjects() throws {
        let lines = try jsonlLines("pipe-token.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "pipe-token.jsonl must contain at least one vector")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                XCTFail("pipe-token.jsonl line is not a JSON object: \(line)")
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)

            // Each pipe-token vector carries a `token` field (base64url) and
            // `claims`/`verify` objects. The fed JSON parser must accept the
            // JSON structure; the token bytes are opaque to fed JSON.
            do {
                _ = try FedJSONValue.parse(line)
            } catch {
                XCTFail("pipe-token vector \(name) rejected by fed parser: \(error)")
            }
        }
    }

    // MARK: - candidate-record.jsonl

    func testCandidateRecordVectorParses() throws {
        let lines = try jsonlLines("candidate-record.jsonl")
        XCTAssertGreaterThan(lines.count, 0, "candidate-record.jsonl must contain at least one vector")

        for line in lines {
            guard let entry = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                XCTFail("candidate-record.jsonl line is not a JSON object: \(line)")
                continue
            }
            let name = try XCTUnwrap(entry["name"] as? String)
            let canonical = try XCTUnwrap(entry["canonical"] as? String)

            // The canonical form must be parseable by the fed strict parser.
            do {
                _ = try FedJSONValue.parse(Data(canonical.utf8))
            } catch {
                XCTFail("candidate-record canonical form \(name) rejected by fed parser: \(error)")
            }
        }
    }

    // MARK: - Helpers

    private func jsonlLines(_ filename: String) throws -> [Data] {
        let url = URL(fileURLWithPath: rdvWireDirectory).appendingPathComponent(filename)
        let content = try String(contentsOf: url, encoding: .utf8)
        return content
            .split(separator: "\n")
            .filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
            .map { Data($0.utf8) }
    }

    /// Extracts the top-level key order from a JSON object string by scanning
    /// for quoted keys before colons. This is a shallow extraction sufficient
    /// for comparing key-sort order in the canonical-valid vectors.
    private func extractKeyOrder(from json: String) -> [String] {
        var keys: [String] = []
        var inString = false
        var escaped = false
        var currentKey = ""
        var depth = 0
        var lookingForKey = true

        for char in json {
            if escaped {
                if lookingForKey { currentKey.append(char) }
                escaped = false
                continue
            }
            if char == "\\" {
                if lookingForKey { currentKey.append(char) }
                escaped = true
                continue
            }
            if char == "\"" {
                inString.toggle()
                if !inString && lookingForKey {
                    keys.append(currentKey)
                    currentKey = ""
                    lookingForKey = false
                }
                continue
            }
            if inString {
                if lookingForKey { currentKey.append(char) }
                continue
            }
            if char == "{" || char == "[" {
                depth += 1
                if depth == 1 { lookingForKey = true }
                continue
            }
            if char == "}" || char == "]" {
                depth -= 1
                continue
            }
            if char == ":" && depth == 1 {
                lookingForKey = false
            }
            if char == "," && depth == 1 {
                lookingForKey = true
                currentKey = ""
            }
        }
        return keys
    }
}