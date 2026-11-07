import Foundation
import XCTest
@testable import SubcClient

private enum BusyTransportError: Error { case noReply }

private final class BusyTransport: Transport {
    var bytes = Data()
    var writes: [Frame] = []
    var windows = true
    var origin: UInt8 = DAEMON_ORIGIN_FLAG
    var refusals = 1
    var loseAfterRefusal = false
    private var channel: UInt16 = 0

    func writeAll(_ data: Data) throws {
        let header = try decodeHeader(Data(data.prefix(HEADER_LEN)))
        let frame = Frame(header: header, body: Data(data.dropFirst(HEADER_LEN)))
        writes.append(frame)
        guard header.ty == .request else { return }
        let body = (try? JSONSerialization.jsonObject(with: frame.body)) as? [String: Any]
        if body?["op"] as? String == "route.open" {
            channel += 1
            var reply: [String: Any] = ["route_channel": channel, "route_epoch": 1]
            if windows { reply["request_window"] = 32; reply["watch_window"] = 24 }
            try respond(frame, type: .response, json: reply)
        } else if refusals > 0 {
            refusals -= 1
            try respond(frame, type: .error, flags: origin, json: ["code": "route_busy", "message": "full", "detail": ["reason": "no_credit"]])
        } else if !loseAfterRefusal {
            let subscription = body?["method"] as? String == "session.subscribe"
            try respond(frame, type: subscription ? .streamEnd : .response, json: [:])
        }
    }

    private func respond(_ frame: Frame, type: FrameType, flags: UInt8 = 0, json: [String: Any]) throws {
        bytes.append(try encodeFrame(ty: type, flags: flags, channel: frame.header.channel,
            epoch: frame.header.epoch, corr: frame.header.corr,
            body: JSONSerialization.data(withJSONObject: json)))
    }

    func readExact(_ n: Int) throws -> Data {
        guard bytes.count >= n else { throw BusyTransportError.noReply }
        let result = Data(bytes.prefix(n))
        bytes.removeFirst(n)
        return result
    }
    func close() {}
}

final class BusyRefusalsTests: XCTestCase {
    private func open(_ client: SubcClient) throws -> RouteHandle {
        try client.routeOpenManagementSurface(moduleId: "example", projectRoot: "/project", harness: "test", session: "session")
    }

    func testTrustedRefusalRetriesWithFreshCorrelationAndSucceeds() throws {
        let transport = BusyTransport()
        var time = 0.0
        let client = SubcClient(transport: transport, busyNow: { time }, busySleep: { time += $0 }, busyRandom: { 0.5 })
        let route = try open(client)
        _ = try client.callManagement(route: route, method: "test")
        let requests = transport.writes.filter { $0.header.channel == route.channel }
        XCTAssertEqual(requests.count, 2)
        XCTAssertNotEqual(requests[0].header.corr, requests[1].header.corr)
        let payload = try JSONSerialization.jsonObject(with: transport.writes[0].body) as? [String: Any]
        XCTAssertEqual(payload?["busy_refusals"] as? Bool, true)
        XCTAssertGreaterThan(time, 0)
    }

    func testForgedAndLegacyRefusalsAreNotRetried() throws {
        for legacy in [false, true] {
            let transport = BusyTransport()
            transport.windows = !legacy
            transport.origin = legacy ? DAEMON_ORIGIN_FLAG : 0
            let client = SubcClient(transport: transport)
            let route = try open(client)
            XCTAssertThrowsError(try client.callManagement(route: route, method: "test")) {
                XCTAssertTrue($0 is SubcError)
                XCTAssertFalse($0 is RouteBusyError)
            }
            XCTAssertEqual(transport.writes.filter { $0.header.channel == route.channel }.count, 1)
        }
    }

    func testRetryCeilingIsThirtySecondsAndNeverForwarded() throws {
        let transport = BusyTransport()
        transport.refusals = 1000
        var time = 0.0
        let client = SubcClient(transport: transport, busyNow: { time }, busySleep: { time += $0 }, busyRandom: { 0.5 })
        let route = try open(client)
        XCTAssertThrowsError(try client.callManagement(route: route, method: "test")) {
            XCTAssertEqual(($0 as? RouteBusyError)?.neverForwarded, true)
            XCTAssertEqual(($0 as? RouteBusyError)?.code, "route_busy")
        }
        XCTAssertEqual(time, 30, accuracy: 0.0001)
        XCTAssertLessThan(transport.writes.count, 100)
    }

    func testCancellationEndsBackoff() async throws {
        let cancelled = await Task {
            let transport = BusyTransport()
            let client = SubcClient(transport: transport, busySleep: { _ in
                withUnsafeCurrentTask { $0?.cancel() }
            })
            do {
                let route = try open(client)
                _ = try client.callManagement(route: route, method: "test")
                return false
            } catch is CancellationError {
                return transport.writes.filter { $0.header.channel != 0 }.count == 1
            } catch { return false }
        }.value
        XCTAssertTrue(cancelled)
    }

    func testForwardedRetryLostIsNotNeverForwarded() throws {
        let transport = BusyTransport()
        transport.loseAfterRefusal = true
        var time = 0.0
        let client = SubcClient(transport: transport, busyNow: { time }, busySleep: { time += $0 })
        let route = try open(client)
        XCTAssertThrowsError(try client.callManagement(route: route, method: "test")) {
            XCTAssertTrue($0 is BusyTransportError)
            XCTAssertFalse($0 is RouteBusyError)
        }
    }

    func testLegacySessionTurnLeavesSubscriptionBitUnset() throws {
        let transport = BusyTransport()
        transport.windows = false
        transport.refusals = 0
        let client = SubcClient(transport: transport)
        _ = try client.runSessionTurn(moduleId: "example", projectRoot: "/project", harness: "test", session: "session", prompt: "hello", provider: "example", model: "example", onEvent: { _ in })
        let requests = transport.writes.filter { $0.header.ty == .request && $0.header.channel != 0 }
        XCTAssertEqual(requests.count, 2)
        XCTAssertTrue(requests.allSatisfy { $0.header.flags & SUBSCRIPTION_FLAG == 0 })
    }

    func testSessionTurnSubscriptionAndSendRetryAndMarkWatch() throws {
        let transport = BusyTransport()
        transport.refusals = 2
        var time = 0.0
        let client = SubcClient(transport: transport, busyNow: { time }, busySleep: { time += $0 })
        _ = try client.runSessionTurn(moduleId: "example", projectRoot: "/project", harness: "test", session: "session", prompt: "hello", provider: "example", model: "example", onEvent: { _ in })
        let requests = transport.writes.filter { $0.header.ty == .request && $0.header.channel != 0 }
        XCTAssertEqual(requests.count, 4)
        XCTAssertEqual(requests.filter { $0.header.flags & 0x80 != 0 }.count, 2)
    }
}
