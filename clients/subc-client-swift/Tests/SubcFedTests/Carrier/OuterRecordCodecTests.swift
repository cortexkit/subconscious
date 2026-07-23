import Foundation
import XCTest
@testable import SubcFed

final class OuterRecordCodecTests: XCTestCase {
    func testTCPRecordUsesLittleEndianLengthAndRoundTrips() throws {
        let payload = Data((0..<257).map { UInt8($0 & 0xff) })
        let record = try FedOuterRecordCodec.encode(payload)
        XCTAssertEqual(record.prefix(4), Data([0x01, 0x01, 0x00, 0x00]))
        XCTAssertEqual(try FedOuterRecordCodec.decodeTCPRecord(record), payload)
    }

    func testTCPDecoderAssemblesSplitAndConcatenatedRecords() throws {
        let first = try FedOuterRecordCodec.encode(Data([1, 2, 3]))
        let second = try FedOuterRecordCodec.encode(Data([4, 5]))
        var decoder = FedTCPRecordDecoder()
        XCTAssertEqual(try decoder.append(first.prefix(2)).count, 0)
        XCTAssertEqual(try decoder.append(Data(first.dropFirst(2)) + second), [Data([1, 2, 3]), Data([4, 5])])
        try decoder.finish()
    }

    func testWebSocketRequiresOneCompleteBinaryRecord() throws {
        let payload = Data([0xaa, 0xbb])
        let record = try FedOuterRecordCodec.encode(payload)
        XCTAssertEqual(try FedOuterRecordCodec.decodeWebSocketMessage(.binary(record)), payload)
        XCTAssertThrowsError(try FedOuterRecordCodec.decodeWebSocketMessage(.text("x"))) { error in
            XCTAssertEqual(error as? FedCarrierError, .webSocketText)
        }
        XCTAssertThrowsError(try FedOuterRecordCodec.decodeWebSocketMessage(.binary(Data()))) { error in
            XCTAssertEqual(error as? FedCarrierError, .webSocketMessageEmpty)
        }
        XCTAssertThrowsError(try FedOuterRecordCodec.decodeWebSocketMessage(.binary(record + record))) { error in
            XCTAssertEqual(error as? FedCarrierError, .webSocketMultipleRecords)
        }
        var mismatch = record
        mismatch[mismatch.startIndex] = 3
        XCTAssertThrowsError(try FedOuterRecordCodec.decodeWebSocketMessage(.binary(mismatch))) { error in
            XCTAssertEqual(error as? FedCarrierError, .webSocketRecordMismatch(declared: 3, actualPayload: 2))
        }
    }

    func testDeclaredLengthIsBoundedBeforeBodyUse() {
        let oversized = Data([0x00, 0x00, 0x01, 0x00])
        XCTAssertThrowsError(try FedOuterRecordCodec.decodeTCPRecord(oversized)) { error in
            XCTAssertEqual(error as? FedCarrierError, .recordTooLarge(declared: 65_536, maximum: 65_535))
        }
    }
}
