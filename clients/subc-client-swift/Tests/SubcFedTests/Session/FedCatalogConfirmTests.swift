import Foundation
import XCTest
@testable import SubcFed

final class FedCatalogConfirmTests: XCTestCase {
    private let features: Set<String> = ["mgmt-v1", "effects-v1", FedFramedBodyCodec.feature]
    private let incarnation = "00000000-0000-4000-8000-0000000000aa"

    private func module(_ id: String, operations: String) -> String {
        "{\"module_id\":\"\(id)\",\"management\":{\"operations\":[\(operations)]}}"
    }

    private func frame(_ body: String, generation: UInt64 = 1) -> FedFrame {
        FedFrame(type: "catalog", fields: ["generation": .integer(generation)], body: Data(body.utf8))
    }

    private func parse(_ body: String) throws -> FedRemoteCatalog {
        let bytes = try FedFrameCodec.encode(frame(body), negotiatedFeatures: features)
        let decoded = try FedFrameCodec.decode(bytes, negotiatedFeatures: features)
        return try FedCatalogCodec.parseRemote(frame: decoded, peerIncarnation: incarnation, peerFeatures: features)
    }

    func testConfirmTrueSurvivesAndAbsentMeansFalse() throws {
        let body = "{\"modules\":[\(module("guarded", operations: #"{"name":"item.open","kind":"query","confirm":true}"#)),\(module("plain", operations: #"{"name":"read","kind":"query"}"#))]}"
        let catalog = try parse(body)
        let guarded = try XCTUnwrap(catalog.lookup(moduleID: "guarded", operation: "item.open"))
        XCTAssertTrue(guarded.confirm)
        let plain = try XCTUnwrap(catalog.lookup(moduleID: "plain", operation: "read"))
        XCTAssertFalse(plain.confirm)
    }

    func testNonBooleanConfirmDropsOnlyThatOperation() throws {
        // `false`, numbers, strings and null are not the boolean `true`,
        // so each drops its operation exactly like a malformed delivery.
        for token in ["false", "0", "1", "\"true\"", "null", "{}", "[]"] {
            let operations = #"{"name":"bad","kind":"query","confirm":\#(token)},{"name":"read","kind":"query"}"#
            let body = "{\"modules\":[\(module("filtered", operations: operations))]}"
            let catalog = try parse(body)
            XCTAssertEqual(catalog.module(id: "filtered")?.operations.map(\.name), ["read"], "token \(token)")
            XCTAssertFalse(try XCTUnwrap(catalog.lookup(moduleID: "filtered", operation: "read")).confirm)
        }
    }

    func testConfirmAloneCannotKeepAnOtherwiseEmptyModule() throws {
        let body = "{\"modules\":[\(module("only-bad", operations: #"{"name":"bad","kind":"query","confirm":"yes"}"#))]}"
        let catalog = try parse(body)
        XCTAssertTrue(catalog.modules.isEmpty)
    }

    func testConfirmSitsBesidePayload() throws {
        let operations = #"{"name":"upload","kind":"mutate","payload":{"max_bytes":1024},"confirm":true}"#
        let body = "{\"modules\":[\(module("up", operations: operations))]}"
        let operation = try XCTUnwrap(parse(body).lookup(moduleID: "up", operation: "upload"))
        XCTAssertTrue(operation.confirm)
        XCTAssertEqual(operation.payload?.maxBytes, 1024)
    }

    func testConfirmWithoutNegotiatedFramedCallsStillParses() throws {
        // The current parser accepts `confirm` without any confirm-specific
        // negotiated feature.
        let body = "{\"modules\":[\(module("guarded", operations: #"{"name":"item.open","kind":"query","confirm":true}"#))]}"
        let bytes = try FedFrameCodec.encode(frame(body), negotiatedFeatures: features)
        let decoded = try FedFrameCodec.decode(bytes, negotiatedFeatures: features)
        let catalog = try FedCatalogCodec.parseRemote(
            frame: decoded, peerIncarnation: incarnation, peerFeatures: ["mgmt-v1"]
        )
        XCTAssertTrue(try XCTUnwrap(catalog.lookup(moduleID: "guarded", operation: "item.open")).confirm)
    }

    func testDirectConstructionKeepsSourceCompatibility() {
        // Existing app code builds and matches without the new member.
        let operation = FedCatalogOperation(name: "read", kind: "query")
        XCTAssertFalse(operation.confirm)
        XCTAssertNil(operation.payload)
        switch operation.delivery {
        case .none:
            break
        case .some:
            XCTFail("delivery should be absent")
        }
    }
}
