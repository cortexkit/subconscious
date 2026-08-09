import Foundation
import XCTest
@testable import SubcFed

/// The rdv-wire side REFUSES fields it does not know, which is the opposite of
/// how the additive projection models behave — and deliberately so. Under
/// rdv-wire §1.2 an unrecognised key is a contract violation rather than a
/// forward-compatible addition, because these rows carry identity and lineage
/// and a silently ignored field could be one that changes their meaning.
///
/// `RdvRegistryRow` is the row with the incident history: it broke twice when
/// the worker added `enrollment_id` and `supersession_generation`. Both times
/// the correct remedy was to TEACH the decoder the new fields, not to relax it —
/// they are decoded as optionals so older rows still parse. Nothing asserted the
/// refusal for this row specifically, though: its `finish()` comes from the
/// shared decoder, so the rule almost certainly held, and "almost certainly
/// holds" is exactly the standard a pin exists to replace.
final class RdvRegistryRowStrictnessTests: XCTestCase {

    private func row(extra: [String: RdvJSONValue] = [:]) -> RdvJSONObject {
        var fields: [String: RdvJSONValue] = [
            "x25519_pubkey_hex": .string(String(repeating: "ab", count: 32)),
            "ed25519_pubkey_hex": .string(String(repeating: "cd", count: 32)),
            "name": .string("galdor"),
            "platform": .string("ios"),
            "candidates": .array([]),
            "last_seen_ms": .string("1783419580000"),
            "online": .boolean(true),
            "reenrolled_after_tombstone": .boolean(false),
        ]
        for (key, value) in extra { fields[key] = value }
        return RdvJSONObject(fields)
    }

    /// The premise: a row carrying exactly the known fields decodes. Without
    /// this, a refusal below could be the decoder rejecting the fixture for an
    /// unrelated reason and the strictness assertions would prove nothing.
    func testAKnownRowDecodes() throws {
        let decoded = try RdvRegistryRow.decode(row())
        XCTAssertEqual(decoded.name, "galdor")
        XCTAssertEqual(decoded.platform, "ios")
        XCTAssertNil(decoded.enrollmentID)
        XCTAssertNil(decoded.supersessionGeneration)
    }

    /// The fields whose absence broke this row twice are optional, so a row
    /// minted before they existed still decodes, and one carrying them is read
    /// rather than refused.
    func testTheFieldsThatBrokeThisRowTwiceAreNowUnderstood() throws {
        let decoded = try RdvRegistryRow.decode(row(extra: [
            "enrollment_id": .string("enr_1"),
            "supersession_generation": .string("7"),
        ]))
        XCTAssertEqual(decoded.enrollmentID, "enr_1")
        XCTAssertEqual(decoded.supersessionGeneration, "7")
    }

    /// The sealing key is decoded BEFORE the producer emits it, so the field can
    /// land server-side without breaking clients already in the field.
    ///
    /// The row-level assertion understates the stake: `RdvRegistrySnapshot.decode`
    /// loops rows with `try` and no per-row recovery, so ONE row carrying an
    /// unconsumed field fails the WHOLE snapshot -- every peer disappears, not
    /// just the new device, on the discovery path.
    func testTheSealingKeyIsUnderstoodBeforeItsProducerEmitsIt() throws {
        // Absent: rows minted before the column exists, which is every row today.
        XCTAssertNil(try RdvRegistryRow.decode(row()).pushSealPubkeyHex)

        // Present: consumed, so `finish()` is satisfied rather than throwing.
        let sealing = String(repeating: "ef", count: 32)
        let decoded = try RdvRegistryRow.decode(row(extra: ["push_seal_pubkey_hex": .string(sealing)]))
        XCTAssertEqual(decoded.pushSealPubkeyHex, sealing)

        // DISTINCT FROM THE TRANSPORT IDENTITY. Sealing to the Noise static would
        // be cross-protocol reuse, and the two keys are adjacent hex strings on
        // one row -- the confusion is a field lookup away.
        XCTAssertNotEqual(decoded.pushSealPubkeyHex, decoded.x25519PubkeyHex)
    }

    /// The snapshot-level statement of the same property, because that is the
    /// blast radius: a row carrying the new field must not fail its NEIGHBOURS.
    func testOneRowCarryingTheNewFieldDoesNotFailTheWholeSnapshot() throws {
        let snapshot = try RdvRegistrySnapshot.decode(RdvJSONObject([
            "type": .string("registry_snapshot"),
            "server_seq": .string("42"),
            "devices": .array([
                .object(row()),
                .object(row(extra: ["push_seal_pubkey_hex": .string(String(repeating: "ef", count: 32))])),
            ]),
        ]))
        XCTAssertEqual(snapshot.devices.count, 2)
        XCTAssertNil(snapshot.devices[0].pushSealPubkeyHex)
        XCTAssertNotNil(snapshot.devices[1].pushSealPubkeyHex)
    }

    /// An unrecognised key must be REFUSED, naming the field. Tolerating it here
    /// would silently drop something the client was never taught to read.
    func testAnUnknownFieldIsRefusedAndNamed() {
        XCTAssertThrowsError(try RdvRegistryRow.decode(row(extra: [
            "some_future_field": .string("value"),
        ]))) { error in
            guard case RdvJSONError.unknownField(let field) = error else {
                return XCTFail("an unknown rdv-wire field must throw unknownField, got \(error)")
            }
            XCTAssertEqual(field, "some_future_field")
        }
    }

    /// Refusal cannot depend on the unknown value's JSON shape: a decoder that
    /// tracked consumed keys only for scalars would pass the test above while
    /// letting a nested object through.
    func testRefusalDoesNotDependOnTheUnknownValuesShape() {
        let shapes: [String: RdvJSONValue] = [
            "future_null": .null,
            "future_bool": .boolean(true),
            "future_object": .object(RdvJSONObject(["nested": .string("x")])),
            "future_array": .array([.string("x")]),
        ]
        for (key, value) in shapes {
            XCTAssertThrowsError(try RdvRegistryRow.decode(row(extra: [key: value])),
                                 "an unknown \(key) must be refused") { error in
                guard case RdvJSONError.unknownField(let field) = error else {
                    return XCTFail("expected unknownField for \(key), got \(error)")
                }
                XCTAssertEqual(field, key)
            }
        }
    }
}
