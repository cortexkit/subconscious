import Foundation
import XCTest
@testable import SubcFed

final class FedCatalogTests: XCTestCase {
    private let features: Set<String> = ["mgmt-v1", "effects-v1", FedFramedBodyCodec.feature]
    private let incarnation = "00000000-0000-4000-8000-0000000000aa"
    private let good = #"{"module_id":"good","management":{"operations":[{"name":"read","kind":"query"}]}}"#

    private func module(_ id: String, operations: String = #"{"name":"read","kind":"query"}"#) -> String {
        "{\"module_id\":\"\(id)\",\"management\":{\"operations\":[\(operations)]}}"
    }

    private func frame(_ body: String, generation: UInt64 = 1) -> FedFrame {
        FedFrame(type: "catalog", fields: ["generation": .integer(generation)], body: Data(body.utf8))
    }

    private func parse(_ body: String) throws -> FedRemoteCatalog {
        // Exercise the stream boundary too: it must not reject entries before
        // the catalog decoder can isolate them from healthy modules.
        let bytes = try FedFrameCodec.encode(frame(body), negotiatedFeatures: features)
        let decoded = try FedFrameCodec.decode(bytes, negotiatedFeatures: features)
        return try FedCatalogCodec.parseRemote(frame: decoded, peerIncarnation: incarnation, peerFeatures: features)
    }

    func testNonObjectEntryDoesNotDisconnectHealthyModule() async throws {
        try await assertSurvivors("{\"modules\":[null,7,[],\(good)]}", ids: ["good"])
    }

    func testDuplicateModuleIDsDropEveryCopyWithoutDisconnecting() async throws {
        let body = "{\"modules\":[\(module("dup")),\(good),\(module("dup", operations: #"{"name":"other","kind":"mutate"}"#))]}"
        try await assertSurvivors(body, ids: ["good"])
    }

    func testOversizedOperationListDoesNotDisconnectHealthyModule() async throws {
        let operations = (0..<257).map { #"{"name":"op\#($0)","kind":"query"}"# }.joined(separator: ",")
        try await assertSurvivors("{\"modules\":[\(module("large", operations: operations)),\(good)]}", ids: ["good"])
    }

    func testDuplicateOperationNamesDropModuleWithoutDisconnecting() async throws {
        let operations = #"{"name":"read","kind":"query"},{"name":"read","kind":"mutate","delivery":false}"#
        try await assertSurvivors("{\"modules\":[\(module("ambiguous", operations: operations)),\(good)]}", ids: ["good"])
    }

    func testModuleCapCountsOnlyUsableModules() async throws {
        let ignored = (0..<10).map { index in
            index.isMultiple(of: 2) ? module("host:\(index)") : module("empty\(index)", operations: "")
        }
        let usable = (0..<59).map { module("module\($0)") } + [good]
        let body = "{\"modules\":[\((ignored + usable).joined(separator: ","))]}"
        XCTAssertEqual(ignored.count + usable.count, 70)
        try await assertSurvivors(body, ids: (0..<59).map { "module\($0)" } + ["good"])
    }

    func testFirst64SurvivingModulesAreKeptInOrder() async throws {
        let entries = [good] + (0..<65).map { module("module\($0)") }
        try await assertSurvivors("{\"modules\":[\(entries.joined(separator: ","))]}",
                                  ids: ["good"] + (0..<63).map { "module\($0)" })
    }

    func testMalformedAndAmbiguousEntriesDoNotConsumeModuleCap() throws {
        let bad = ["false", module("dup"), module("dup"), module("empty", operations: ""),
                   module("bad", operations: #"{"name":"read","kind":"query"},{"name":"read","kind":"query"}"#)]
        let usable = (0..<64).map { module("module\($0)") }
        let catalog = try parse("{\"modules\":[\((bad + usable).joined(separator: ","))]}")
        XCTAssertEqual(catalog.modules.map(\.moduleID), (0..<64).map { "module\($0)" })
    }

    func testDuplicateIDBeyondCapStillRemovesEarlierCopy() throws {
        let entries = [module("dup")] + (0..<64).map { module("module\($0)") } + [module("dup", operations: "")]
        let catalog = try parse("{\"modules\":[\(entries.joined(separator: ","))]}")
        XCTAssertEqual(catalog.modules.map(\.moduleID), (0..<64).map { "module\($0)" })
    }

    func testInvalidOperationNamesAndKindsAreStillDropped() async throws {
        let ops = #"{"name":"bad:name","kind":"query"},{"name":"bad","kind":"unknown"},{"name":"bad","kind":"unknown"},{"name":"read","kind":"query"}"#
        try await assertSurvivors("{\"modules\":[\(module("filtered", operations: ops)),\(good)]}", ids: ["filtered", "good"])
        XCTAssertEqual(try parse("{\"modules\":[\(module("filtered", operations: ops))]}").modules.first?.operations.map(\.name), ["read"])
    }

    func testOversizedToolListDropsOnlyItsModule() async throws {
        let tools = Array(repeating: "{}", count: 257).joined(separator: ",")
        let large = #"{"module_id":"large","tools":[\#(tools)],"management":{"operations":[{"name":"read","kind":"query"}]}}"#
        try await assertSurvivors("{\"modules\":[\(large),\(good)]}", ids: ["good"])
    }

    func testWrongFrameTypeStillThrows() {
        assertBadFrame(FedFrame(type: "hello", fields: ["generation": .integer(1)], body: Data(#"{"modules":[]}"#.utf8)))
    }

    func testMissingOrNonIntegerGenerationStillThrows() {
        for fields: FedJSONObject in [[:], ["generation": .string("1")], ["generation": .number(1.5)],
                                      ["generation": .integer(FedJSONValue.firstUnsafeInteger)]] {
            assertBadFrame(FedFrame(type: "catalog", fields: fields.dictionary, body: Data(#"{"modules":[]}"#.utf8)))
        }
    }

    func testNonJSONObjectBodyStillThrows() {
        for body in ["not json", "[]", "null", "1", #"{"modules":[],}"#] {
            assertBadFrame(frame(body))
        }
    }

    func testMissingOrNonArrayModulesStillThrows() {
        for body in ["{}", #"{"modules":{}}"#, #"{"modules":null}"#] {
            assertBadFrame(frame(body))
        }
    }

    private func assertBadFrame(_ frame: FedFrame, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try FedCatalogCodec.parseRemote(frame: frame, peerIncarnation: incarnation, peerFeatures: features),
                             file: file, line: line) {
            XCTAssertEqual($0 as? FedFailure, .protocolViolation(byeCode: "fed_bad_frame"), file: file, line: line)
        }
    }

    func testOpaqueSchemasAcceptInt64BoundsAndKeepModuleUsable() async throws {
        let bounds = #"{"minimum":-9223372036854775808,"maximum":9223372036854775807}"#
        let properties = (0..<31).map { "\"field\($0)\":\(bounds)" }.joined(separator: ",")
        let schema = #"{"properties":{\#(properties)},"additionalNumbers":[-5,1.0,1e3,1e400]}"#
        let opaque = #"{"module_id":"opaque","tools":[{"name":"inspect","schema":\#(schema)}],"management":{"config_schema":\#(schema),"operations":[{"name":"read","kind":"query"}]}}"#
        try await assertSurvivors("{\"modules\":[\(opaque),\(good)]}", ids: ["opaque", "good"])
    }

    func testRoleOwnedDeclarationsAcceptOpaqueNumbers() async throws {
        let entry = #"{"module_id":"roles","role_declarations":{"worker":{"nested":[{"minimum":-9223372036854775808,"maximum":9223372036854775807},1.0,1e3,1e400]}},"management":{"operations":[{"name":"read","kind":"query"}]}}"#
        try await assertSurvivors("{\"modules\":[\(entry),\(good)]}", ids: ["roles", "good"])
    }

    func testUnrepresentablePayloadMaximumDropsOnlyOperation() async throws {
        for token in ["-5", "-9223372036854775808", "9223372036854775807", "1e3", "1.0", "1e400"] {
            let operations = #"{"name":"bad","kind":"query","payload":{"max_bytes":\#(token)}},{"name":"read","kind":"query"}"#
            let body = "{\"modules\":[\(module("filtered", operations: operations)),\(good)]}"
            XCTAssertEqual(try parse(body).module(id: "filtered")?.operations.map(\.name), ["read"])
            try await assertSurvivors(body, ids: ["filtered", "good"])
        }
    }

    func testUnknownOperationMembersRemainCallable() async throws {
        let operations = #"{"name":"extended","kind":"query","future":-5,"payload":{"max_bytes":1024,"future":[9223372036854775807]}},{"name":"read","kind":"query"}"#
        let body = "{\"modules\":[\(module("filtered", operations: operations)),\(good)]}"
        let operation = try XCTUnwrap(parse(body).lookup(moduleID: "filtered", operation: "extended"))
        XCTAssertEqual(operation.payload?.maxBytes, 1024)
        XCTAssertEqual(try parse(body).module(id: "filtered")?.operations.map(\.name), ["extended", "read"])
        try await assertSurvivors(body, ids: ["filtered", "good"], calls: [("filtered", "extended")])
    }

    func testUnknownModuleMembersKeepBothModulesUsable() async throws {
        let operations = #""operations":[{"name":"read","kind":"query"}]"#
        let negative = #"{"module_id":"negative","future":-5,"management":{\#(operations)}}"#
        let oversized = #"{"module_id":"oversized","future":9223372036854775807,"management":{\#(operations)}}"#
        try await assertSurvivors("{\"modules\":[\(negative),\(oversized),\(good)]}",
                                  ids: ["negative", "oversized", "good"],
                                  calls: [("negative", "read"), ("oversized", "read")])
    }

    func testUnreadNestedModuleMembersDoNotHideModule() async throws {
        let operations = #""operations":[{"name":"read","kind":"query"}]"#
        for fields in [#""future":{"nested":[9223372036854775807]},"management":{\#(operations)}"#,
                       #""management":{"future":[1e3],\#(operations)}"#,
                       #""tools":[-5,{"schema":{},"future":-9223372036854775808}],"management":{\#(operations)}"#,
                       #""config_schema":{"minimum":-9223372036854775808},"management":{\#(operations)}"#] {
            let entry = "{\"module_id\":\"extended\",\(fields)}"
            try await assertSurvivors("{\"modules\":[\(entry),\(good)]}", ids: ["extended", "good"], calls: [("extended", "read")])
        }
    }

    func testInvalidModuleVersionRemainsOptional() async throws {
        for version in ["-5", "9223372036854775807", #"{"nested":-5}"#, "false"] {
            let entry = #"{"module_id":"versioned","module_version":\#(version),"management":{"operations":[{"name":"read","kind":"query"}]}}"#
            let body = "{\"modules\":[\(entry),\(good)]}"
            XCTAssertNil(try parse(body).module(id: "versioned")?.moduleVersion)
            try await assertSurvivors(body, ids: ["versioned", "good"], calls: [("versioned", "read")])
        }
    }

    func testInvalidReadOperationFieldsSkipOnlyTheirOperation() async throws {
        let operations = #"{"name":-5,"kind":"query"},{"name":"bad-kind","kind":9223372036854775807},{"name":"bad-delivery","kind":"query","delivery":-5},{"name":"read","kind":"query"}"#
        let body = "{\"modules\":[\(module("filtered", operations: operations)),\(good)]}"
        XCTAssertEqual(try parse(body).module(id: "filtered")?.operations.map(\.name), ["read"])
        try await assertSurvivors(body, ids: ["filtered", "good"], calls: [("filtered", "read")])
    }

    func testUnrepresentableNumbersOutsideModulesStillThrow() {
        for token in ["-9223372036854775808", "9223372036854775807", "1e3", "1.0", "1e400"] {
            assertBadFrame(frame("{\"modules\":[\(good)],\"future\":{\"nested\":[\(token)]}}"))
            assertBadFrame(frame("{\"modules\":[\(good)],\"generation\":\(token)}"))
        }
    }

    func testUnrepresentableHeaderGenerationStillThrowsAtFramingBoundary() {
        for token in ["-9223372036854775808", "9223372036854775807", "1e3", "1.5"] {
            let header = Data("{\"type\":\"catalog\",\"generation\":\(token)}".utf8)
            XCTAssertThrowsError(try FedFrameCodec.encode(headerData: header, body: Data(#"{"modules":[]}"#.utf8)))
        }
    }

    func testValidNonIntegralProtocolNumberDoesNotDropModule() async throws {
        let entry = #"{"module_id":"fraction","future":1.5,"management":{"operations":[{"name":"read","kind":"query"}]}}"#
        try await assertSurvivors("{\"modules\":[\(entry),\(good)]}", ids: ["fraction", "good"])
    }

    func testOpaqueSchemasStillRequireValidJSONSyntax() {
        for token in ["01", "NaN", "1e", "--1", "1."] {
            assertBadFrame(frame(#"{"modules":[{"module_id":"bad","management":{"config_schema":{"minimum":\#(token)}}}]}"#))
        }
    }

    private func assertSurvivors(
        _ body: String, ids: [String], calls: [(moduleID: String, method: String)] = [("good", "read")],
        file: StaticString = #filePath, line: UInt = #line
    ) async throws {
        let catalog = try parse(body)
        XCTAssertEqual(catalog.modules.map(\.moduleID), ids, file: file, line: line)
        for id in ids {
            XCTAssertEqual(catalog.lookup(moduleID: id, operation: "read")?.kind, "query", file: file, line: line)
        }

        let transport = FedLoopbackByteTransport()
        let engine = FedSessionEngine(deps: .init(
            transport: transport, store: FedMemoryStateStore(), clock: FedFakeClock(),
            localPublicKey: Data(repeating: 0x11, count: 32), responderStaticPublicKey: Data(repeating: 0x22, count: 32),
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "c", count: 32)
        ))
        let establish = Task { try await engine.establish() }
        try await waitUntil {
            try await transport.sentFrames(negotiationComplete: false).contains { $0.knownType == .hello }
        }
        let hello = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(features: features.sorted()), incarnation: incarnation,
            ledgerEpoch: "00000000-0000-4000-8000-0000000000bb", connectionAttemptID: String(repeating: "d", count: 32)
        )
        await transport.enqueueInbound(try FedFrameCodec.encode(hello))
        try await waitUntil {
            try await transport.sentFrames(negotiationComplete: true, features: self.features).contains { $0.knownType == .catalog }
        }
        await transport.enqueueInbound(try FedFrameCodec.encode(frame(body), negotiationComplete: true, negotiatedFeatures: features))
        try await establish.value
        let phase = await engine.currentPhase
        XCTAssertEqual(phase, .ready, file: file, line: line)
        let initial = await engine.remoteCatalog
        XCTAssertEqual(initial?.modules.map(\.moduleID), ids, file: file, line: line)

        // Later snapshots use the same isolation policy as the initial exchange.
        let update = try FedFrameCodec.encode(frame(body, generation: 2), negotiationComplete: true, negotiatedFeatures: features)
        let received = try await engine.processInboundBytes(update)
        XCTAssertEqual(received.count, 1, file: file, line: line)
        let updated = await engine.remoteCatalog
        XCTAssertEqual(updated?.generation, 2, file: file, line: line)
        XCTAssertEqual(updated?.modules.map(\.moduleID), ids, file: file, line: line)
        let updatedPhase = await engine.currentPhase
        XCTAssertEqual(updatedPhase, .ready, file: file, line: line)
        for call in calls {
            let prepared = try await engine.prepareManagementCall(moduleID: call.moduleID, method: call.method, params: [:],
                policy: FedAdmissionPolicySnapshot(defaultDeadlineMs: 60_000))
            try await engine.dispatchPreparedCall(prepared)
        }
        let sent = try await transport.sentFrames(negotiationComplete: true, features: features)
        for call in calls {
            let sentCall = try XCTUnwrap(sent.first { $0.knownType == .call && $0.header["module"] == .string(call.moduleID) }, file: file, line: line)
            XCTAssertEqual(try FedJSONObject(jsonData: sentCall.body)["method"], .string(call.method), file: file, line: line)
        }
        await engine.disconnect()
    }

    private func waitUntil(_ predicate: @escaping () async throws -> Bool) async throws {
        let start = DispatchTime.now().uptimeNanoseconds
        while try await !predicate() {
            guard DispatchTime.now().uptimeNanoseconds - start < 2_000_000_000 else {
                XCTFail("timeout waiting for session progress")
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            try await Task.sleep(nanoseconds: 2_000_000)
        }
    }
}
