import Foundation
import XCTest
@testable import SubcFed

final class RemotePairingStateTests: XCTestCase {
    typealias Object = RemotePairingVectors.Object

    private func scenarios(_ kind: String) throws -> [Object] {
        try XCTUnwrap(RemotePairingVectors.json("state-machine.json")[kind] as? [Object])
    }

    func testEveryCeremonyVectorAtTheDeviceBoundary() async throws {
        let root = try RemotePairingVectors.json("state-machine.json")
        let prefixes = try XCTUnwrap(root["prefixes"] as? [String: [[Any]]])
        func expand(_ events: [[Any]]) throws -> [[Any]] {
            try events.flatMap { event in
                guard event.first as? String == "use" else { return [event] }
                return try expand(XCTUnwrap(prefixes[XCTUnwrap(event.last as? String)]))
            }
        }
        let rows = try scenarios("ceremonies")
        XCTAssertEqual(rows.count, 26)
        for row in rows {
            let steps = try expand(XCTUnwrap(row["steps"] as? [[Any]]))
            let expected = try XCTUnwrap(row["expect"] as? Object)
            let name = try XCTUnwrap(row["name"] as? String)
            try await exerciseCeremony(steps, expected: expected, name: name)
        }
    }

    /// Only D is implemented here. T-only events (roster changes, C's confirm,
    /// rate limits) are projected to the bytes or close D can observe.
    private func exerciseCeremony(_ steps: [[Any]], expected: Object, name: String) async throws {
        func first(_ action: String) -> [Any]? { steps.first { $0.first as? String == action } }
        let end = expected["end"] as? String
        let releases = expected["releases"] as? Int ?? 0
        let pin = expected["pin"] as? Bool ?? false
        let reverse = (expected["owners"] as? [String: String])?["D"] == "r"
        let fixture = try await RemotePairingFixture(targetLower: reverse)
        let pipe = RemotePairingPipe()
        let client = try fixture.client(pipes: [pipe])
        let targetCarrier: any FedNoiseMessageCarrier = reverse ? pipe.target : FedTCPRecordCarrier(stream: pipe.target)
        let admits = (expected["message_2"] as? Int ?? 0) > 0
        let firstWasResult = first("first_t_frame") != nil
        let appBinding = first("binding")?.dropFirst().first as? String == "window_app"
        let hasBinding = first("binding") != nil
        let commitBeforeBinding = steps.firstIndex { $0.first as? String == "commit" }.map { index in
            index < (steps.firstIndex { $0.first as? String == "binding" } ?? Int.max)
        } ?? false
        let resultLabel = first("result")?.last as? String
        let localMatch = first("match") != nil && end != "restart"
        let target = Task { () -> String? in
            if !admits {
                _ = try await targetCarrier.receiveNoiseMessage()
                await targetCarrier.close()
                return nil
            }
            let mode: FedRemotePairMode = reverse ? .targetRelay : .direct
            let (wire, hash) = try await fixture.targetWire(targetCarrier, mode: mode)
            if firstWasResult {
                try await wire.send(fixture.result("granted"))
            } else if appBinding {
                try await wire.send(RemotePairingFixture.binding(.app))
                let bye = try await wire.receive()
                XCTAssertEqual(bye.byeCode, "fed_class_not_allowed", name)
                XCTAssertEqual(try fixture.store.load(scope: fixture.scope), FedRemotePairReceipt.consumed, name)
            } else if hasBinding && !commitBeforeBinding && releases < 2 {
                try await wire.send(RemotePairingFixture.binding())
                let commit = try await wire.receive()
                XCTAssertEqual(commit.typeName, FedRemotePairCodec.commit, name)
                XCTAssertNil(try fixture.store.load(scope: fixture.scope), name)
            } else if hasBinding && !commitBeforeBinding && end != "crossed_legs" {
                let (_, code) = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: mode, pipe: pipe)
                if end == "restart" { return code }
                if localMatch {
                    let answer = try await wire.receive()
                    XCTAssertEqual(answer.header["match"], .boolean(true), name)
                }
                if let resultLabel { try await wire.send(fixture.result(resultLabel)) }
                else if end == "rotation" { await wire.close() }
                else { try await wire.send(fixture.result("not_granted")) }
                return code
            }
            await wire.close()
            return nil
        }

        let pending: FedPendingRemotePairing?
        do {
            if reverse {
                let attempt = FedRemotePairAttempt(targetKey: fixture.targetKey.publicKey, store: fixture.store, scope: fixture.scope)
                let dial = fixture.dial()
                let link = try await dial.respondOnRelay(pipe.device, attempt: attempt)
                pending = try await attempt.exchange(link, dial: dial, deviceClass: .phone, provider: nil, resultTimeout: .seconds(2))
            } else {
                pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
            }
        } catch {
            XCTAssertFalse(pin, name)
            if appBinding { XCTAssertEqual(error as? FedRemotePairingError, .wrongClass, name) }
            if firstWasResult { XCTAssertEqual(error as? FedRemotePairingError, .protocolViolation, name) }
            pending = nil
        }
        if let pending {
            if end == "restart" {
                await pending.cancel()
                let restarted = try fixture.client(pipes: [])
                let receipt = try await restarted.hasRemotePairingReceipt(rendezvous: fixture.rendezvous)
                XCTAssertFalse(receipt, name)
            } else {
                if !localMatch {
                    try await fedTestWithTimeout {
                        while !(await pipe.closed) { await Task.yield() }
                    }
                }
                do {
                    let outcome = try await pending.confirmMatch(true)
                    if case .granted = outcome { XCTAssertTrue(pin, name) }
                    else { XCTAssertFalse(pin, name) }
                } catch {
                    XCTAssertFalse(pin, name)
                    if resultLabel == "granted_wrong_key" { XCTAssertEqual(error as? FedRemotePairingError, .wrongTargetKey, name) }
                    if resultLabel == "granted", !localMatch { XCTAssertEqual(error as? FedRemotePairingError, .protocolViolation, name) }
                }
            }
            let code = try await target.value
            XCTAssertEqual(pending.code, code, name)
        } else {
            _ = try await target.value
            if !admits || firstWasResult || commitBeforeBinding || !hasBinding {
                let writes = await pipe.writes
                XCTAssertEqual(writes[0], 1, "D sends nothing after message 1 without a valid binding: \(name)")
            }
        }
        await fixture.rendezvous.disconnect()
    }

    func testEveryStatusSessionVectorAtTheDeviceBoundary() async throws {
        let rows = try scenarios("status_sessions")
        XCTAssertEqual(rows.count, 7)
        for row in rows {
            let expected = try XCTUnwrap(row["expect"] as? [Any])
            let name = try XCTUnwrap(row["name"] as? String)
            let matched = try XCTUnwrap(row["local_match"] as? Bool)
            let requests = try XCTUnwrap(row["requests"] as? [String])
            let fixture = try await RemotePairingFixture()
            let result = try await checkStatus(fixture, matched: matched,
                generationDelta: 0, wrongHash: requests == ["status_wrong_hash"],
                targetReply: row["receipt"] as? String, extraFrame: requests.count > 1)
            if expected.last as? Bool == true { XCTAssertEqual(result, "pinned", name) }
            else if row["receipt"] == nil || row["receipt"] is NSNull { XCTAssertEqual(result, "statusUnavailable", name) }
            else if requests.count > 1 { XCTAssertEqual(result, "protocolViolation", name) }
            else if !matched { XCTAssertEqual(result, "storageUnavailable", name) }
            else if expected.first as? String == "granted_wrong_key" { XCTAssertEqual(result, "wrongTargetKey", name) }
            else { XCTAssertEqual(result, "not_granted", name) }
            await fixture.rendezvous.disconnect()
        }
    }

    func testEveryReceiptLifecycleVectorAtTheDeviceBoundary() async throws {
        let rows = try scenarios("receipt_lifecycle")
        XCTAssertEqual(rows.count, 8)
        for row in rows {
            let fixture = try await RemotePairingFixture()
            let fallback = try fixture.client(pipes: [])
            let steps = try XCTUnwrap(row["steps"] as? [[Any]])
            let expected = try XCTUnwrap(row["expect"] as? [String])
            let name = try XCTUnwrap(row["name"] as? String)
            var outputs: [String] = []
            var script = TargetReceiptScript()
            for step in steps {
                let verb = try XCTUnwrap(step.first as? String)
                switch verb {
                case "remember":
                    try fixture.store.replace(scope: fixture.scope, record: JSONEncoder().encode(receiptFor(fixture)))
                case "start":
                    // Even an unavailable endpoint must supersede the receipt.
                    do {
                        _ = try await fallback.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
                        XCTFail("no target carrier in the new attempt")
                    } catch { }
                case "ceremony_not_granted", "ceremony_bye":
                    try await exerciseTerminalCeremony(fixture, bye: verb == "ceremony_bye")
                case "check_local":
                    let retained = try await fallback.hasRemotePairingReceipt(rendezvous: fixture.rendezvous)
                    outputs.append(retained ? "retained" : "deleted")
                default:
                    let local = try await fallback.hasRemotePairingReceipt(rendezvous: fixture.rendezvous)
                    guard let reply = try script.step(step, local: local) else { continue }
                    switch reply {
                    case "wrong_target":
                        // A replacement device can get the TCP connection, but
                        // cannot answer message 1 for the receipt's saved key.
                        try await refusesReplacementTarget(fixture)
                        outputs.append(reply)
                    case "pinned" where local, "not_granted" where local:
                        let status = try await checkStatus(fixture, targetReply: reply == "pinned" ? "granted" : reply)
                        outputs.append(status)
                    case "granted", "not_granted":
                        // No prior local match for this generation exists in
                        // this row: probe the serving result without granting
                        // authority to D absent a receipt of its own.
                        XCTAssertFalse(local, name)
                        do {
                            _ = try await fallback.remotePairingStatus(rendezvous: fixture.rendezvous)
                            XCTFail("status ran without a lookup receipt: \(name)")
                        } catch { XCTAssertEqual(error as? FedRemotePairingError, .noReceipt, name) }
                        outputs.append(reply)
                    default: outputs.append(reply)
                    }
                }
            }
            XCTAssertEqual(outputs, expected, name)
            await fixture.rendezvous.disconnect()
        }
    }

    private func receiptFor(_ fixture: RemotePairingFixture, generationDelta: UInt64 = 0,
                            wrongHash: Bool = false, matched: Bool = true) -> FedRemotePairReceipt {
        let window = RemotePairingFixture.window(.phone)
        return FedRemotePairReceipt(windowID: window.id, generation: window.generation + generationDelta,
            transcriptHash: Data(repeating: wrongHash ? 0x54 : 0x53, count: 32), targetKey: fixture.targetKey.publicKey, matched: matched)
    }

    private func exerciseTerminalCeremony(_ fixture: RemotePairingFixture, bye: Bool) async throws {
        let pipe = RemotePairingPipe()
        let client = try fixture.client(pipes: [pipe])
        let target = Task {
            let (wire, hash) = try await fixture.targetWire(FedTCPRecordCarrier(stream: pipe.target))
            _ = try await fixture.exchangeAsTarget(wire: wire, handshakeHash: hash, mode: .direct, pipe: pipe)
            _ = try await wire.receive()
            try await wire.send(bye
                ? FedFrame(type: "bye", fields: ["code": .string("fed_pairing_declined")]) : fixture.result("not_granted"))
        }
        let pending = try await client.pair(remote: fixture.target, rendezvous: fixture.rendezvous)
        do {
            let result = try await pending.confirmMatch(true)
            XCTAssertFalse(bye)
            XCTAssertEqual(result, .notGranted)
        } catch { XCTAssertEqual(error as? FedRemotePairingError, .refused(code: "fed_pairing_declined")) }
        try await target.value
        XCTAssertNil(try fixture.store.load(scope: fixture.scope))
    }

    private func refusesReplacementTarget(_ fixture: RemotePairingFixture) async throws {
        let pipe = RemotePairingPipe()
        let client = try fixture.client(pipes: [pipe])
        let target = Task {
            let carrier = FedTCPRecordCarrier(stream: pipe.target)
            let message1 = try await carrier.receiveNoiseMessage()
            let replacement = try FedNoiseKeyPair(privateKey: Data(repeating: 0x82, count: 32))
            let peer = try FedNoiseIKResponder(staticKey: replacement, prologue: FedRemotePairMode.direct.prologue(status: true))
            XCTAssertThrowsError(try peer.readMessage1(message1))
            await carrier.close()
        }
        do {
            _ = try await client.remotePairingStatus(rendezvous: fixture.rendezvous)
            XCTFail("status must authenticate the saved T key")
        } catch { XCTAssertEqual(error as? FedRemotePairingError, .statusUnavailable) }
        try await target.value
        let retained = try await client.hasRemotePairingReceipt(rendezvous: fixture.rendezvous)
        XCTAssertTrue(retained)
    }

    private func checkStatus(_ fixture: RemotePairingFixture, matched: Bool = true, generationDelta: UInt64 = 0,
                             wrongHash: Bool = false, targetReply: String?, extraFrame: Bool = false) async throws -> String {
        let pipe = RemotePairingPipe()
        let client = try fixture.client(pipes: [pipe])
        let receipt = receiptFor(fixture, generationDelta: generationDelta, wrongHash: wrongHash, matched: matched)
        try fixture.store.replace(scope: fixture.scope, record: JSONEncoder().encode(receipt))
        let target = matched ? Task {
            let carrier = FedTCPRecordCarrier(stream: pipe.target)
            guard let targetReply else {
                _ = try await carrier.receiveNoiseMessage()
                await carrier.close()
                return
            }
            let (wire, _) = try await fixture.targetWire(carrier, status: true)
            let request = try await wire.receive()
            XCTAssertEqual(request, receipt.request)
            let response = fixture.result(wrongHash ? "not_granted" : targetReply)
            if extraFrame {
                let extra = FedFrame(type: FedRemotePairCodec.match, fields: ["match": .boolean(true)])
                try await wire.session.sendTransportPayload(FedRemotePairCodec.encode(response) + FedRemotePairCodec.encode(extra))
            } else { try await wire.send(response) }
        } : nil
        let result: String
        do {
            switch try await client.remotePairingStatus(rendezvous: fixture.rendezvous) {
            case .granted(let grant):
                XCTAssertEqual(grant.computerStaticPublicKey, fixture.targetKey.publicKey)
                result = "pinned"
            case .notGranted: result = "not_granted"
            case .checkStatus: XCTFail("status is not a ceremony"); result = "unexpected"
            }
        } catch { result = String(describing: error) }
        try await target?.value
        if result == "pinned" || result == "not_granted" {
            XCTAssertNil(try fixture.store.load(scope: fixture.scope))
        } else { XCTAssertNotNil(try fixture.store.load(scope: fixture.scope)) }
        return result
    }
}

/// C/T-only conditions are scripted at the test boundary; no roster or receipt
/// writes on T are part of the Swift carrier.
private struct TargetReceiptScript {
    private struct Saved {
        let status: String
        let grant: String
        let pairingID: String?
        let ended: Int
    }
    private var receipts: [String: [Saved]] = [:]
    private var roster: [String: String] = [:]
    private var sessions: [String: Int] = [:]
    private var chargedAt: [String: Int] = [:]

    mutating func step(_ step: [Any], local: Bool) throws -> String? {
        let action = try XCTUnwrap(step[0] as? String)
        let device = try XCTUnwrap(step[1] as? String)
        switch action {
        case "receipt":
            receipts[device, default: []].append(Saved(status: try XCTUnwrap(step[2] as? String),
                grant: try XCTUnwrap(step[3] as? String), pairingID: step[4] as? String, ended: try XCTUnwrap(step[5] as? Int)))
        case "roster": roster[device] = try XCTUnwrap(step[2] as? String)
        case "unpair", "tombstone", "re_pair":
            receipts[device] = nil
            roster[device] = action == "re_pair" ? step[2] as? String : nil
        case "m1":
            let now = try XCTUnwrap(step[2] as? Int)
            if let start = sessions[device], now - start >= 5 { sessions[device] = nil }
            guard sessions[device] == nil,
                  receipts[device, default: []].contains(where: { (0..<86_400).contains(now - $0.ended) }) else { return "zero_bytes" }
            sessions[device] = now
            return "message_2"
        case "request":
            let now = try XCTUnwrap(step[2] as? Int)
            guard step[3] as? Bool == true else { return "unauthenticated" }
            guard let start = sessions.removeValue(forKey: device), now - start < 5 else { return "request_timeout" }
            if local, step[5] as? String != "fixture_target" { return "wrong_target" }
            if let charge = chargedAt[device], now - charge < 60 { return "rate_limited" }
            chargedAt[device] = now
            let status = try XCTUnwrap(step[4] as? String)
            let grant = receipts[device, default: []].contains {
                $0.status == status && now - $0.ended < 86_400 && $0.grant == "granted" && roster[device] == $0.pairingID
            }
            if !grant { return "not_granted" }
            return local && status == "status" ? "pinned" : "granted"
        default: XCTFail("unrecognized receipt step: \(action)")
        }
        return nil
    }
}
