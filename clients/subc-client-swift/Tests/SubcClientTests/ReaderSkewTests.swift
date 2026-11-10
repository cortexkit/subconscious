import Foundation
import Darwin
import XCTest
@testable import SubcClient

private enum ReaderTransportError: Error { case noReply }

private final class ReaderTransport: Transport, CapabilityProbeTransport {
    var capabilityProbeDeadline: TimeInterval?
    var bytes = Data()
    var writes: [Frame] = []
    var closed = false
    var onWrite: ((Frame, ReaderTransport) throws -> Void)?

    func writeAll(_ data: Data) throws {
        let header = try decodeHeader(Data(data.prefix(HEADER_LEN)))
        let frame = Frame(header: header, body: Data(data.dropFirst(HEADER_LEN)))
        writes.append(frame)
        try onWrite?(frame, self)
    }

    func enqueue(_ frame: Frame, type: FrameType = .response, json: [String: Any], channel: UInt16 = 0,
                 epoch: UInt32 = 0, corr: UInt64? = nil) throws {
        bytes.append(try encodeFrame(ty: type, flags: 0, channel: channel, epoch: epoch,
                                     corr: corr ?? frame.header.corr,
                                     body: JSONSerialization.data(withJSONObject: json)))
    }

    func readExact(_ n: Int) throws -> Data {
        guard bytes.count >= n else { throw ReaderTransportError.noReply }
        let result = Data(bytes.prefix(n))
        bytes.removeFirst(n)
        return result
    }

    func close() { closed = true }
}

final class ReaderSkewTests: XCTestCase {
    private let target: [String: Any] = [
        "incarnation": "daemon", "kind": "spawn", "spawn": 17, "authority_epoch": 3,
        "first_party": true, "approved_declarations_digest": "approved",
    ]

    private func open(_ client: SubcClient) throws -> RouteHandle {
        try client.routeOpenManagementSurface(moduleId: "module", projectRoot: "/work",
                                              harness: "test", session: "session")
    }

    private func routeTransport(target: Any? = nil) -> ReaderTransport {
        let transport = ReaderTransport()
        transport.onWrite = { frame, transport in
            var body: [String: Any] = ["route_channel": 7, "route_epoch": 2]
            body["target"] = target
            try transport.enqueue(frame, json: body)
        }
        return transport
    }

    private func catalog(_ modules: [[String: Any]]) throws -> [CatalogEntry] {
        let transport = ReaderTransport()
        transport.onWrite = { frame, transport in try transport.enqueue(frame, json: ["modules": modules]) }
        let entries = try SubcClient(transport: transport).catalogList(moduleId: "module")
        let request = try XCTUnwrap(try JSONSerialization.jsonObject(with: transport.writes[0].body) as? [String: Any])
        XCTAssertEqual(request["module_id"] as? String, "module")
        return entries
    }

    func testReaderSkewCatalogFieldsAndLegacyAbsence() throws {
        let entries = try catalog([
            ["module_id": "module", "incarnation": "daemon", "spawn": MAX_ATTESTATION_INTEGER,
             "first_party": true, "origin": "release", "requested_declarations_digest": "requested",
             "approved_declarations_digest": "approved", "approved_declarations": ["provider": ["key": true]],
             "requested_declarations": ["provider": ["future": [1, 2, 3]]]],
            ["module_id": "legacy"],
        ])
        XCTAssertEqual(entries[0].incarnation, "daemon")
        XCTAssertEqual(entries[0].spawn, MAX_ATTESTATION_INTEGER)
        XCTAssertEqual(entries[0].firstParty, true)
        XCTAssertEqual(entries[0].origin, .release)
        XCTAssertEqual(entries[0].requestedDeclarationsDigest, "requested")
        XCTAssertEqual(entries[0].approvedDeclarationsDigest, "approved")
        XCTAssertEqual((entries[0].approvedDeclarations?["provider"] as? [String: Bool])?["key"], true)
        XCTAssertEqual((entries[0].requestedDeclarations?["provider"] as? [String: [Int]])?["future"], [1, 2, 3])
        XCTAssertNil(entries[1].incarnation)
        XCTAssertNil(entries[1].spawn)
        XCTAssertNil(entries[1].firstParty)
        XCTAssertNil(entries[1].origin)
        XCTAssertNil(entries[1].requestedDeclarationsDigest)
        XCTAssertNil(entries[1].approvedDeclarationsDigest)
        XCTAssertNil(entries[1].approvedDeclarations)
        XCTAssertNil(entries[1].requestedDeclarations)
    }

    func testReaderSkewCatalogRejectsUnsafeSpawn() throws {
        for value: Any in [MAX_ATTESTATION_INTEGER + 1, UInt64.max, -1, true, 1.25] {
            XCTAssertThrowsError(try catalog([["module_id": "module", "spawn": value]]), "accepted \(value)")
        }
    }

    func testReaderSkewUnknownOriginReadsAsOther() throws {
        let entries = try catalog(["future", "other", "development", "release"].map {
            ["module_id": "module", "origin": $0]
        })
        XCTAssertEqual(entries.map(\.origin), [.other, .other, .development, .release])
    }

    func testReaderSkewRouteTargetIsRetainedAndLegacyAbsent() throws {
        let route = try open(SubcClient(transport: routeTransport(target: target)))
        let decoded = try XCTUnwrap(route.target)
        XCTAssertEqual(decoded.incarnation, "daemon")
        XCTAssertEqual(decoded.kind, .spawn)
        XCTAssertEqual(decoded.spawn, 17)
        XCTAssertEqual(decoded.authorityEpoch, 3)
        XCTAssertTrue(decoded.firstParty)
        XCTAssertEqual(decoded.approvedDeclarationsDigest, "approved")
        XCTAssertNil(try open(SubcClient(transport: routeTransport())).target)
    }

    func testReaderSkewIncompleteTargetIsUnmarked() throws {
        for key in target.keys {
            var incomplete = target
            incomplete.removeValue(forKey: key)
            XCTAssertNil(try open(SubcClient(transport: routeTransport(target: incomplete))).target, key)
        }
        for malformed: Any in [NSNull(), "future", ["first_party": 1]] {
            XCTAssertNil(try open(SubcClient(transport: routeTransport(target: malformed))).target)
        }
    }

    func testReaderSkewUnknownTargetKindCannotRetainSpawnOrMark() throws {
        for kind in ["future", "prefix", "unsupervised"] {
            var body = target
            body["kind"] = kind
            let decoded = try XCTUnwrap(try open(SubcClient(transport: routeTransport(target: body))).target)
            XCTAssertNotEqual(decoded.kind, .spawn)
            XCTAssertFalse(decoded.firstParty)
            XCTAssertNil(decoded.spawn)
            XCTAssertNil(decoded.authorityEpoch)
            XCTAssertNil(decoded.approvedDeclarationsDigest)
        }
    }

    func testReaderSkewTargetRejectsUnsafeSpawnAndEpoch() throws {
        for key in ["spawn", "authority_epoch"] {
            for value: Any in [MAX_ATTESTATION_INTEGER + 1, UInt64.max, -1, true, 1.25] {
                var body = target
                body[key] = value
                XCTAssertThrowsError(try open(SubcClient(transport: routeTransport(target: body))), "\(key)=\(value)")
                body.removeValue(forKey: "incarnation")
                XCTAssertThrowsError(try open(SubcClient(transport: routeTransport(target: body))))
            }
            var safe = target
            safe[key] = MAX_ATTESTATION_INTEGER
            XCTAssertNotNil(try open(SubcClient(transport: routeTransport(target: safe))).target)
        }
    }

    func testReaderSkewAuthorityChangedIsReopenable() throws {
        let transport = routeTransport()
        let client = SubcClient(transport: transport)
        let route = try open(client)
        transport.onWrite = { frame, transport in
            try transport.enqueue(frame, type: .push,
                                  json: ["op": "route.closed", "channels": [7], "module_id": "module",
                                         "reason": "authority_changed", "drained": true, "abandoned": 0], corr: 0)
            transport.bytes.append(try encodeFrame(ty: .goodbye, flags: 0, channel: route.channel,
                                                   epoch: route.epoch, corr: 0, body: Data()))
        }
        XCTAssertThrowsError(try client.callManagement(route: route, method: "read")) { error in
            XCTAssertEqual((error as? SubcError)?.closeReason?.rawValue, "authority_changed")
            XCTAssertEqual((error as? SubcError)?.closeReason?.mayReopen, true)
        }
        XCTAssertEqual(client.routeCloseReason(for: route)?.mayReopen, true)
        let unknown = try XCTUnwrap(RouteControlPush(body: Data(#"{"op":"route.closing","reason":"future"}"#.utf8)))
        XCTAssertFalse(unknown.reason.mayReopen)
    }

    func testReaderSkewStaleEpochRefusalDecodes() throws {
        let transport = ReaderTransport()
        transport.onWrite = { frame, transport in
            try transport.enqueue(frame, type: .error, json: ["code": "stale_authority_epoch", "message": "reopen"])
        }
        XCTAssertThrowsError(try open(SubcClient(transport: transport))) { error in
            XCTAssertEqual((error as? SubcError)?.code, "stale_authority_epoch")
            XCTAssertEqual((error as? SubcError)?.routeOpenMayRetry, true)
            XCTAssertFalse(error is RouteBusyError)
        }
        XCTAssertEqual(transport.writes.count, 1, "reader does not retry opens automatically")
    }

    private func negotiatedTransport(describe: @escaping (Frame, ReaderTransport) throws -> Void) -> ReaderTransport {
        let transport = ReaderTransport()
        var channel: UInt16 = 0
        transport.onWrite = { frame, transport in
            let body = try JSONSerialization.jsonObject(with: frame.body) as? [String: Any]
            if body?["op"] as? String == "server.describe" {
                try describe(frame, transport)
            } else {
                channel += 1
                try transport.enqueue(frame, json: ["route_channel": channel, "route_epoch": 1])
            }
        }
        return transport
    }

    func testReaderSkewCloseReasonsSentOnEveryNegotiatedOpen() throws {
        let transport = negotiatedTransport { frame, transport in
            try transport.enqueue(frame, json: ["capabilities": [ROUTE_CLOSE_REASONS_CAPABILITY]])
        }
        let client = SubcClient.connected(transport: transport)
        _ = try open(client)
        _ = try open(client)
        XCTAssertEqual(transport.writes.count, 3, "describe is cached per connection")
        for frame in transport.writes.dropFirst() {
            let body = try XCTUnwrap(try JSONSerialization.jsonObject(with: frame.body) as? [String: Any])
            XCTAssertEqual(body["accepts_close_reasons"] as? [String], ACCEPTED_ROUTE_CLOSE_REASONS)
            XCTAssertTrue((body["accepts_close_reasons"] as? [String] ?? []).contains("authority_changed"))
        }
    }

    func testReaderSkewCloseReasonsOmittedWithoutCapability() throws {
        for describe: [String: Any] in [[:], ["capabilities": []], ["capabilities": ["future/v1"]],
                                        ["capabilities": ROUTE_CLOSE_REASONS_CAPABILITY]] {
            let transport = negotiatedTransport { frame, transport in try transport.enqueue(frame, json: describe) }
            let client = SubcClient.connected(transport: transport)
            _ = try open(client)
            _ = try open(client)
            for frame in transport.writes.dropFirst() {
                let body = try XCTUnwrap(try JSONSerialization.jsonObject(with: frame.body) as? [String: Any])
                XCTAssertNil(body["accepts_close_reasons"])
            }
        }
    }

    func testReaderSkewDescribeFailuresDoNotCostConnection() throws {
        for failure in ["refused", "unparseable", "unreadable"] {
            let transport = negotiatedTransport { frame, transport in
                if failure == "refused" {
                    try transport.enqueue(frame, type: .error, json: ["code": "unknown_op"])
                } else if failure == "unparseable" {
                    transport.bytes.append(try encodeFrame(ty: .response, flags: 0, channel: 0, epoch: 0,
                                                           corr: frame.header.corr, body: Data("bad json".utf8)))
                }
            }
            let client = SubcClient.connected(transport: transport)
            XCTAssertFalse(transport.closed, failure)
            XCTAssertNil(transport.capabilityProbeDeadline, "probe budget must not leak into route I/O")
            XCTAssertEqual(try open(client).channel, 1, failure)
            let request = try XCTUnwrap(try JSONSerialization.jsonObject(with: transport.writes[1].body) as? [String: Any])
            XCTAssertNil(request["accepts_close_reasons"], failure)
        }
    }

    func testReaderSkewSilentDescribeIsBoundedAndLaterRouteStillOpens() throws {
        XCTAssertEqual(CAPABILITY_PROBE_TIMEOUT, 2)
        var sockets: [Int32] = [-1, -1]
        XCTAssertEqual(socketpair(AF_UNIX, SOCK_STREAM, 0, &sockets), 0)
        let transport = POSIXTransport(connectedSocket: sockets[0])
        let peer = sockets[1]
        let served = expectation(description: "route answered after silent describe")
        DispatchQueue.global().async {
            let server = POSIXTransport(connectedSocket: peer)
            defer { server.close(); served.fulfill() }
            do {
                let describe = try readFrame(from: server)
                let describeBody = try JSONSerialization.jsonObject(with: describe.body) as? [String: Any]
                XCTAssertEqual(describeBody?["op"] as? String, "server.describe")
                // Deliberately never answer describe. A later route request must
                // still arrive on this socket after the probe budget expires.
                let route = try readFrame(from: server)
                let body = try JSONSerialization.jsonObject(with: route.body) as? [String: Any]
                XCTAssertEqual(body?["op"] as? String, "route.open")
                XCTAssertNil(body?["accepts_close_reasons"])
                try server.writeAll(encodeFrame(ty: .response, flags: 0, channel: 0, epoch: 0,
                                                corr: route.header.corr,
                                                body: JSONSerialization.data(withJSONObject: ["route_channel": 1, "route_epoch": 1])))
            } catch { XCTFail("socket daemon failed: \(error)") }
        }
        let start = ProcessInfo.processInfo.systemUptime
        let client = SubcClient.connected(transport: transport)
        let elapsed = ProcessInfo.processInfo.systemUptime - start
        XCTAssertGreaterThanOrEqual(elapsed, CAPABILITY_PROBE_TIMEOUT - 0.1)
        // Poll's millisecond granularity and scheduler delay are not I/O budget.
        XCTAssertLessThan(elapsed, CAPABILITY_PROBE_TIMEOUT + 0.5)
        XCTAssertNil(transport.capabilityProbeDeadline)
        XCTAssertEqual(try open(client).channel, 1)
        client.close()
        wait(for: [served], timeout: 5)
    }

    func testReaderSkewPartialProbeFrameSurvivesTimeout() throws {
        let body = try JSONSerialization.data(withJSONObject: ["capabilities": [ROUTE_CLOSE_REASONS_CAPABILITY]])
        let frame = try encodeFrame(ty: .response, flags: 0, channel: 0, epoch: 0, corr: 1, body: body)
        for split in [FROZEN_PREFIX_LEN - 1, FROZEN_PREFIX_LEN + 2, HEADER_LEN + 2] {
            var sockets: [Int32] = [-1, -1]
            XCTAssertEqual(socketpair(AF_UNIX, SOCK_STREAM, 0, &sockets), 0)
            let transport = POSIXTransport(connectedSocket: sockets[0])
            let server = POSIXTransport(connectedSocket: sockets[1])
            defer { transport.close(); server.close() }
            try server.writeAll(Data(frame.prefix(split)))
            transport.capabilityProbeDeadline = ProcessInfo.processInfo.systemUptime + 0.01
            XCTAssertThrowsError(try readFrame(from: transport))
            transport.capabilityProbeDeadline = nil
            try server.writeAll(Data(frame.dropFirst(split)))
            let resumed = try readFrame(from: transport)
            XCTAssertEqual(resumed.body, body)
            XCTAssertEqual(resumed.header.corr, 1)
        }
    }

    func testReaderSkewCatalogWatchInitialAndCoalescedPushesCancel() throws {
        let transport = ReaderTransport()
        transport.onWrite = { frame, transport in
            guard frame.header.ty == .request else { return }
            let body = try JSONSerialization.jsonObject(with: frame.body) as? [String: Any]
            XCTAssertEqual(body?["op"] as? String, "catalog.watch")
            XCTAssertEqual(frame.header.channel, 0)
            XCTAssertEqual(frame.header.epoch, 0)
            // Ignore another request's event, then read the initial snapshot
            // and a coalesced advance without expecting a separate ACK.
            try transport.enqueue(frame, type: .streamData, json: ["op": "catalog.watch", "generation": 99], corr: 88)
            try transport.enqueue(frame, type: .response, json: ["op": "catalog.watch", "generation": 0])
            try transport.enqueue(frame, type: .streamData, json: ["op": "catalog.watch", "generation": UInt64.max])
        }
        var generations: [UInt64] = []
        try SubcClient(transport: transport).catalogWatch { generation in
            generations.append(generation)
            return generations.count < 2
        }
        XCTAssertEqual(generations, [0, UInt64.max])
        XCTAssertEqual(transport.writes.map(\.header.ty), [.request, .cancel])
        XCTAssertEqual(transport.writes[0].header.corr, transport.writes[1].header.corr)
        XCTAssertEqual(transport.writes[1].header.channel, 0)
    }

    func testReaderSkewCatalogWatchTerminalAndMalformedPushes() throws {
        let transport = ReaderTransport()
        transport.onWrite = { frame, transport in
            transport.bytes.append(try encodeFrame(ty: .streamEnd, flags: 0, channel: 0, epoch: 0,
                                                   corr: frame.header.corr, body: Data()))
        }
        try SubcClient(transport: transport).catalogWatch { _ in XCTFail("unexpected generation"); return true }
        XCTAssertEqual(transport.writes.count, 1, "a terminated watch does not need cancellation")
        for body in [#"{"op":"other","generation":1}"#, #"{"op":"catalog.watch","generation":true}"#,
                     #"{"op":"catalog.watch"}"#, #"{"op":"catalog.watch","generation":-1}"#] {
            XCTAssertThrowsError(try CatalogWatchEvent(body: Data(body.utf8)))
        }
    }

    func testReaderSkewExistingPublicCallShapesRemainUsable() throws {
        let makeError: (String) -> SubcError = SubcError.init(message:)
        XCTAssertEqual(makeError("legacy").message, "legacy")
        let client = SubcClient(transport: ReaderTransport())
        let list: (AdmissionClass) throws -> [CatalogEntry] = client.catalogList(admissionClass:)
        _ = list
    }
}
