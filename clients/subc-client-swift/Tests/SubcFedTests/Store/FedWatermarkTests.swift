import XCTest
@testable import SubcFed

final class FedWatermarkTests: XCTestCase {
    private let responder = Data(repeating: 7, count: 32)

    private func record(_ seq: UInt64, _ disposition: FedEffectDisposition, incarnation: String = "inc") -> FedUnresolvedEffectRecord {
        FedUnresolvedEffectRecord(
            effect: FedEffectID(incarnation: incarnation, seq: seq),
            responderStaticPublicKey: responder,
            phase: disposition == .unknown ? .sent : .terminal,
            disposition: disposition
        )
    }

    /// The rule as it was first written: walk every sequence number from 1.
    /// Kept as the reference the linear version must agree with.
    private func reference(_ records: [FedUnresolvedEffectRecord], incarnation: String) -> UInt64 {
        let settled = records.filter { $0.effect.incarnation == incarnation && $0.isSettled }.map(\.effect.seq)
        guard let maxSettled = settled.max(), maxSettled > 0 else { return 0 }
        var watermark: UInt64 = 0
        for seq in 1...maxSettled {
            let matches = records.filter { $0.effect.incarnation == incarnation && $0.effect.seq == seq }
            if matches.isEmpty || matches.allSatisfy(\.isSettled) {
                watermark = seq
            } else {
                break
            }
        }
        return watermark
    }

    func testAgreesWithTheReferenceOnRandomDocuments() {
        var generator = SystemRandomNumberGenerator()
        let dispositions: [FedEffectDisposition] = [.recorded, .notSent, .ambiguous, .unknown]
        for _ in 0..<2_000 {
            let count = Int.random(in: 0...12, using: &generator)
            let records = (0..<count).map { _ in
                record(
                    UInt64.random(in: 1...20, using: &generator),
                    dispositions.randomElement(using: &generator)!,
                    incarnation: Bool.random(using: &generator) ? "inc" : "old"
                )
            }
            XCTAssertEqual(
                FedWatermark.contiguousSettledPrefix(of: records, incarnation: "inc"),
                reference(records, incarnation: "inc"),
                "records: \(records.map { ($0.effect.incarnation, $0.effect.seq, $0.disposition.rawValue) })"
            )
        }
    }

    func testNamedCases() {
        XCTAssertEqual(FedWatermark.contiguousSettledPrefix(of: [], incarnation: "inc"), 0)
        // A gap is not a hold: seqs 2 and 4 belong to other destinations.
        XCTAssertEqual(FedWatermark.contiguousSettledPrefix(of: [record(1, .recorded), record(3, .ambiguous), record(5, .notSent)], incarnation: "inc"), 5)
        // An unsettled record holds the watermark just below it.
        XCTAssertEqual(FedWatermark.contiguousSettledPrefix(of: [record(1, .recorded), record(3, .unknown), record(5, .recorded)], incarnation: "inc"), 2)
        // A settled and an unsettled record at the same seq: the seq is not settled.
        XCTAssertEqual(FedWatermark.contiguousSettledPrefix(of: [record(4, .recorded), record(4, .unknown), record(6, .recorded)], incarnation: "inc"), 3)
        // Unsettled beyond every settled record does not hold it back.
        XCTAssertEqual(FedWatermark.contiguousSettledPrefix(of: [record(2, .recorded), record(9, .unknown)], incarnation: "inc"), 2)
        // Another incarnation's records count neither as settled nor as unsettled.
        XCTAssertEqual(FedWatermark.contiguousSettledPrefix(of: [record(1, .unknown, incarnation: "old"), record(3, .recorded)], incarnation: "inc"), 3)
    }

    /// The phone that measured 8.7 s per mutation held a few hundred records
    /// with sequence numbers near 519,000. The cost must depend on the number
    /// of records, not on how large their sequence numbers are.
    func testLargeSequenceNumbersCostNothingExtra() {
        let records = (0..<600).map { index in
            record(519_000 + UInt64(index) * 3, index == 599 ? .unknown : .recorded)
        }
        let started = Date()
        let watermark = FedWatermark.contiguousSettledPrefix(of: records, incarnation: "inc")
        // The unsettled record is past every settled one, so it holds nothing
        // back: the watermark is the highest settled sequence number.
        XCTAssertEqual(watermark, 519_000 + 598 * 3)
        XCTAssertLessThan(Date().timeIntervalSince(started), 1.0)
    }
}
