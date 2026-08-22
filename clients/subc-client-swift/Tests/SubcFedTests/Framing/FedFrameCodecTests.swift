import Foundation
import XCTest
@testable import SubcFed

final class FedFrameCodecTests: XCTestCase {
    func testLogicalStreamHandlesSplitAndCoalescedFrames() throws {
        let catalog = try FedFrameCodec.encode(
            type: "catalog",
            fields: ["generation": .integer(1)],
            body: Data(#"{"modules":[]}"#.utf8)
        )
        let bye = try FedFrameCodec.encode(
            type: "bye",
            fields: ["code": .string("fed_goodbye")]
        )
        var decoder = FedFrameStreamDecoder()
        XCTAssertTrue(try decoder.append(catalog.prefix(3)).isEmpty)
        let frames = try decoder.append(Data(catalog.dropFirst(3)) + bye)
        XCTAssertEqual(frames.count, 2)
        XCTAssertEqual(frames[0].typeName, "catalog")
        XCTAssertEqual(frames[0].body, Data(#"{"modules":[]}"#.utf8))
        XCTAssertEqual(frames[1].typeName, "bye")
        try decoder.finish()
    }

    func testBodylessTypesRejectBodiesAndCatalogUsesOneMiBCap() throws {
        let byeHeader = Data(#"{"code":"fed_goodbye","type":"bye"}"#.utf8)
        var bodyless = littleEndian(UInt32(byeHeader.count))
        bodyless.append(byeHeader)
        bodyless.append(littleEndian(1))
        bodyless.append(0)
        XCTAssertThrowsError(try FedFrameCodec.decode(bodyless)) { error in
            XCTAssertEqual(error as? FedFrameError, .bodylessFrame(type: "bye", declared: 1))
        }

        let catalogHeader = Data(#"{"generation":1,"type":"catalog"}"#.utf8)
        var oversizedCatalog = littleEndian(UInt32(catalogHeader.count))
        oversizedCatalog.append(catalogHeader)
        oversizedCatalog.append(littleEndian(1_048_577))
        XCTAssertThrowsError(try FedFrameCodec.decode(oversizedCatalog)) { error in
            XCTAssertEqual(error as? FedFrameError, .catalogBodyTooLarge(declared: 1_048_577, maximum: 1_048_576))
        }
    }

    func testHeaderAndBodyDeclarationsAreBoundedBeforeAllocation() throws {
        XCTAssertThrowsError(try FedFrameCodec.decode(Data([0x01, 0x00, 0x01, 0x00]))) { error in
            XCTAssertEqual(error as? FedFrameError, .headerTooLarge(declared: 65_537, maximum: 65_536))
        }

        let header = Data(#"{"code":"fed_goodbye","type":"bye"}"#.utf8)
        var oversized = littleEndian(UInt32(header.count))
        oversized.append(header)
        oversized.append(littleEndian(FedFrameCodec.defaultMaximumBodyLength + 1))
        XCTAssertThrowsError(try FedFrameCodec.decode(oversized)) { error in
            XCTAssertEqual(error as? FedFrameError, .bodyTooLarge(
                declared: FedFrameCodec.defaultMaximumBodyLength + 1,
                maximum: FedFrameCodec.defaultMaximumBodyLength
            ))
        }
    }

    func testUnknownTypesAreRejectedBeforeNegotiationAndIgnoredAfterwards() throws {
        let unknown = try FedFrameCodec.encode(
            type: "future.frame",
            fields: ["version": .integer(1)],
            body: Data([1, 2, 3]),
            negotiationComplete: true
        )
        XCTAssertThrowsError(try FedFrameCodec.decode(unknown)) { error in
            XCTAssertEqual(error as? FedFrameError, .unknownTypeBeforeNegotiation("future.frame"))
        }
        XCTAssertTrue(try FedFrameCodec.decodeFrames(unknown, negotiationComplete: true).isEmpty)
    }

    func testPinnedHeaderAndBodyBytesCoverBaselineManagementAndEffectsForms() throws {
        let incarnation = "00000000-0000-4000-8000-000000000000"
        let baselineHeader = Data(#"{"type":"bye","code":"fed_goodbye"}"#.utf8)
        let baseline = try FedFrameCodec.encode(headerData: baselineHeader)
        XCTAssertEqual(baseline, framedBytes(header: baselineHeader, body: Data()))

        let managementHeader = Data(("{\"type\":\"call\",\"effect\":{\"incarnation\":\"" + incarnation + "\",\"seq\":1},\"module\":\"prefrontal-core\",\"surface\":\"management\",\"deadline_ms\":300000}").utf8)
        let managementBody = Data(#"{"method":"board.state","params":{}}"#.utf8)
        let management = try FedFrameCodec.encode(
            headerData: managementHeader,
            body: managementBody,
            negotiatedFeatures: ["mgmt-v1"]
        )
        XCTAssertEqual(management, framedBytes(header: managementHeader, body: managementBody))

        let effectsHeader = Data(("{\"type\":\"call\",\"effect\":{\"incarnation\":\"" + incarnation + "\",\"seq\":2},\"module\":\"rooms\",\"mutating\":true,\"deadline_ms\":300000}").utf8)
        let effectsBody = Data(#"{"name":"post","arguments":{}}"#.utf8)
        let effects = try FedFrameCodec.encode(
            headerData: effectsHeader,
            body: effectsBody,
            negotiatedFeatures: ["effects-v1"]
        )
        XCTAssertEqual(effects, framedBytes(header: effectsHeader, body: effectsBody))
    }

    func testStrictCatalogBodyRejectsDuplicateKeysBeforeFrameDelivery() throws {
        let header = Data(#"{"generation":1,"type":"catalog"}"#.utf8)
        let body = Data(#"{"modules":[{"tools":[],"tools":[]},]}"#.utf8)
        var bytes = littleEndian(UInt32(header.count))
        bytes.append(header)
        bytes.append(littleEndian(UInt32(body.count)))
        bytes.append(body)
        XCTAssertThrowsError(try FedFrameCodec.decode(bytes)) { error in
            guard case .invalidCatalog(.duplicateKey("tools")) = error as? FedFrameError else {
                return XCTFail("unexpected error: \(error)")
            }
        }
    }

    private func framedBytes(header: Data, body: Data) -> Data {
        var bytes = littleEndian(UInt32(header.count))
        bytes.append(header)
        bytes.append(littleEndian(UInt32(body.count)))
        bytes.append(body)
        return bytes
    }

    private func littleEndian(_ value: UInt32) -> Data {
        var value = value.littleEndian
        return withUnsafeBytes(of: &value) { Data($0) }
    }
}
