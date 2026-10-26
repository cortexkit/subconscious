import CryptoKit
import Foundation
import SQLite3
import XCTest
@testable import SubcFed

final class DeviceApprovalNoticeTests: XCTestCase {
    private var vectorDirectory: URL {
        URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../../../vectors/rdv-device-approval-notices-v1").standardizedFileURL
    }

    private func rows() throws -> [RdvJSONObject] {
        let bytes = try Data(contentsOf: vectorDirectory.appendingPathComponent("rdv-notices-v1.0.jsonl"))
        return try bytes.split(separator: 10).map { try RdvJSONValue.parseObject(Data($0)) }
    }

    private func object(_ row: RdvJSONObject, _ member: String) throws -> RdvJSONObject {
        guard case .object(let object)? = row[member] else { throw FedRendezvousError.malformedMessage }
        return object
    }

    private func bytes(_ object: RdvJSONObject) throws -> Data { try RdvCanonicalJSON.canonicalize(.object(object)) }

    private func pin(_ row: RdvJSONObject) throws -> RdvAccountSigningKeyPin {
        var fields = RdvFieldDecoder(try object(row, "account_signing_key"))
        let id = try fields.string("key_id")
        let key = try Curve25519.Signing.PublicKey(rawRepresentation: Data(hex: fields.string("ed25519_pubkey_hex")))
        return RdvAccountSigningKeyPin(keyId: id, ed25519PublicKey: key)
    }

    private func notice(_ index: Int = 0) throws -> DeviceApprovalNotice {
        let row = try rows()[index]
        return try DeviceApprovalNotice.verify(bytes(object(row, "frame")), pin: pin(row))
    }

    func testPublishedSetChecksumAndEveryNoticeSignatureFingerprintCodeAndAck() throws {
        let data = try Data(contentsOf: vectorDirectory.appendingPathComponent("rdv-notices-v1.0.jsonl"))
        let hash = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(hash, "b42af5617e6f3fdd0db4d1d5a7bb2e1544265be418a5af6e1119cd153dc30ecb")
        XCTAssertEqual(try String(contentsOf: vectorDirectory.appendingPathComponent("SHA256SUMS")), "\(hash)  rdv-notices-v1.0.jsonl\n")
        let all = try rows()
        XCTAssertEqual(all.count, 10)
        var types: Set<String> = []
        for (index, row) in all.enumerated() {
            let frame = try object(row, "frame")
            let payload = try object(frame, "payload")
            if case .string(let type)? = payload["type"] { types.insert(type) }
            let value = try DeviceApprovalNotice.verify(bytes(frame), pin: pin(row))
            XCTAssertEqual(value.serverSeq, String(101 + index))
            let expectedAck = try DeviceApprovalNoticeAck.decode(object(row, "ack"))
            let ack = try DeviceApprovalNoticeAck(notice: value, seq: String(index + 1))
            XCTAssertEqual(ack, expectedAck)
            XCTAssertEqual(try RdvJSONValue.parseObject(Data(ack.encode().utf8)), try object(row, "ack"))
            let target: DeviceApprovalTarget?
            switch value.item {
            case .enrollment(let view): target = view.target
            case .removal(let view): target = view.target
            case .merge, .resolved: target = nil
            }
            if let target {
                let machine = target.roster == .machine
                XCTAssertEqual(target.fingerprint.hex, machine
                    ? "d7970dfb76fb5257abfc1d7ca4f1ceabf40a0e9e10b97cdd3db5f4b729f14b20"
                    : "8b5391699779104ad11d3b6f1eac7bd57ff27610650560a3177a3da8b9dc8dd9")
                XCTAssertEqual(target.fingerprint.comparisonCode, machine ? "8133-2194-9783" : "8770-3785-4794")
            }
            var tampered = try bytes(frame)
            let signaturePrefix = Data("\"sig_hex\":\"".utf8)
            let range = try XCTUnwrap(tampered.range(of: signaturePrefix))
            tampered[range.upperBound] = tampered[range.upperBound] == 48 ? 49 : 48
            XCTAssertThrowsError(try DeviceApprovalNotice.verify(tampered, pin: pin(row)))
            let wrongPin = RdvAccountSigningKeyPin(keyId: "other-account-key", ed25519PublicKey: try pin(row).ed25519PublicKey)
            XCTAssertThrowsError(try DeviceApprovalNotice.verify(bytes(frame), pin: wrongPin))
        }
        XCTAssertEqual(types.count, 6)
    }

    func testClosedNoticeAndAcknowledgementObjectsAndDecimalDomains() throws {
        for row in try rows() {
            let payload = try object(object(row, "frame"), "payload")
            var invalid = payload.storage
            invalid["extra"] = .null
            XCTAssertThrowsError(try DeviceApprovalNotice.decode(RdvJSONObject(invalid)))
            for bad: RdvJSONValue in [.string("0"), .string("01"), .string("-1"), .string("18446744073709551616"), .boolean(true)] {
                invalid = payload.storage; invalid["server_seq"] = bad
                XCTAssertThrowsError(try DeviceApprovalNotice.decode(RdvJSONObject(invalid)))
            }
            var ack = try object(row, "ack").storage
            ack["extra"] = .null
            XCTAssertThrowsError(try DeviceApprovalNoticeAck.decode(RdvJSONObject(ack)))
            ack = try object(row, "ack").storage; ack["state_version"] = .string("0")
            XCTAssertThrowsError(try DeviceApprovalNoticeAck.decode(RdvJSONObject(ack)))
            let raw = try String(decoding: bytes(payload), as: UTF8.self)
            let duplicate = "{\"notice_id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAX\"," + raw.dropFirst()
            XCTAssertThrowsError(try RdvJSONValue.parseObject(Data(duplicate.utf8)))
            XCTAssertThrowsError(try RdvJSONValue.parseObject(Data(raw.replacingOccurrences(of: "\"server_seq\":\"", with: "\"server_seq\":1,\"ignored\":\"").utf8)))
        }
        var pending = try object(object(rows()[0], "frame"), "payload").storage
        var enrollment = try object(RdvJSONObject(pending), "enrollment").storage
        enrollment["fingerprint"] = .string(String(repeating: "0", count: 64))
        pending["enrollment"] = .object(RdvJSONObject(enrollment))
        XCTAssertThrowsError(try DeviceApprovalNotice.decode(RdvJSONObject(pending))) { error in
            XCTAssertEqual((error as? DeviceApprovalError)?.code, .fingerprintMismatch)
        }
        enrollment = try object(object(object(rows()[0], "frame"), "payload"), "enrollment").storage
        enrollment["code"] = .string("8770-3785-4794")
        pending["enrollment"] = .object(RdvJSONObject(enrollment))
        XCTAssertThrowsError(try DeviceApprovalNotice.decode(RdvJSONObject(pending)))
    }

    func testPushParserAcceptsOnlyFixedWakeupHint() throws {
        for kind in ["device-approval-v1", "device-removal-v1", "device-merge-v1"] {
            let json = "{\"aps\":{\"alert\":\"A security action is waiting for review\"},\"kind\":\"\(kind)\",\"notice_id\":\"01ARZ3NDEKTSV4RRFFQ69G5FAX\"}"
            let push = try DeviceApprovalPush(json: Data(json.utf8))
            XCTAssertEqual(push.kind, kind)
            XCTAssertEqual(push.noticeID, "01ARZ3NDEKTSV4RRFFQ69G5FAX")
            for bad in [
                json.replacingOccurrences(of: "security action", with: "device approval"),
                json.replacingOccurrences(of: "\"alert\":", with: "\"mutable-content\":1,\"alert\":"),
                json.replacingOccurrences(of: "\"kind\":", with: "\"extra\":null,\"kind\":"),
                json.replacingOccurrences(of: "\"kind\":", with: "\"kind\":\"\(kind)\",\"kind\":"),
                json.replacingOccurrences(of: kind, with: "unknown-v1"),
                json.replacingOccurrences(of: "01ARZ3NDEKTSV4RRFFQ69G5FAX", with: "bad-id"),
                json.replacingOccurrences(of: "\"aps\":", with: "\"mutable-content\":1,\"aps\":"),
            ] { XCTAssertThrowsError(try DeviceApprovalPush(json: Data(bad.utf8))) }
        }
    }

    func testNoticeAndAckSequenceMaximumAndNamedRefusals() throws {
        var payload = try object(object(rows()[0], "frame"), "payload").storage
        let maximum = "9007199254740991"
        let excessive = "9007199254740992"
        payload["server_seq"] = .string(maximum)
        let value = try DeviceApprovalNotice.decode(RdvJSONObject(payload))
        XCTAssertEqual(value.serverSeq, maximum)
        XCTAssertEqual(try DeviceApprovalNoticeAck(notice: value, seq: maximum).seq, maximum)
        payload["server_seq"] = .string(excessive)
        XCTAssertThrowsError(try DeviceApprovalNotice.decode(RdvJSONObject(payload))) { error in
            XCTAssertEqual(error as? RdvSequenceError, RdvSequenceError(field: "server_seq", value: excessive))
        }
        XCTAssertThrowsError(try DeviceApprovalNoticeAck(notice: value, seq: excessive)) { error in
            XCTAssertEqual(error as? RdvSequenceError, RdvSequenceError(field: "seq", value: excessive))
        }
        payload["server_seq"] = .string(maximum)
        var enrollment = try object(RdvJSONObject(payload), "enrollment").storage
        enrollment["state_version"] = .string(excessive)
        payload["enrollment"] = .object(RdvJSONObject(enrollment))
        XCTAssertThrowsError(try DeviceApprovalNotice.decode(RdvJSONObject(payload)))
    }

    func testValidSignaturesDoNotPermitUnknownNoticeOrViewMembers() throws {
        let key = try RdvWireFixtures.signingKey()
        let pin = RdvAccountSigningKeyPin(keyId: key.keyId, ed25519PublicKey: key.publicKey)
        let original = try object(object(rows()[0], "frame"), "payload")
        var payload = original.storage
        payload["extra"] = .string("authenticated but forbidden")
        let signedPayload = try RdvTestSigning.signedEnvelopeText(signPayload: RdvJSONObject(payload))
        XCTAssertThrowsError(try DeviceApprovalNotice.verify(Data(signedPayload.utf8), pin: pin)) { error in
            XCTAssertEqual(error as? RdvJSONError, .unknownField("extra"))
        }
        payload = original.storage
        var enrollment = try object(original, "enrollment").storage
        enrollment["extra"] = .string("authenticated but forbidden")
        payload["enrollment"] = .object(RdvJSONObject(enrollment))
        let signedView = try RdvTestSigning.signedEnvelopeText(signPayload: RdvJSONObject(payload))
        XCTAssertThrowsError(try DeviceApprovalNotice.verify(Data(signedView.utf8), pin: pin)) { error in
            XCTAssertEqual(error as? DeviceApprovalError, DeviceApprovalError(code: .unknownMember, field: "extra"))
        }
    }

    func testCapabilitiesOptInOutCanonicalBodies() throws {
        XCTAssertEqual(DeviceCapabilitiesUpdate.method, "PUT")
        XCTAssertEqual(DeviceCapabilitiesUpdate.path, "/v1/device/capabilities")
        let added = try DeviceCapabilitiesUpdate.approvals(enabled: true, current: ["fed-serve", "other-v1"])
        XCTAssertEqual(String(decoding: added.body, as: UTF8.self), "{\"capabilities\":[\"device-approval-v1\",\"fed-serve\",\"other-v1\"]}")
        XCTAssertEqual(try DeviceCapabilitiesUpdate.approvals(enabled: true, current: added.capabilities).body, added.body)
        let removed = try DeviceCapabilitiesUpdate.approvals(enabled: false, current: added.capabilities)
        XCTAssertEqual(String(decoding: removed.body, as: UTF8.self), "{\"capabilities\":[\"fed-serve\",\"other-v1\"]}")
        XCTAssertEqual(String(decoding: try DeviceCapabilitiesUpdate.approvals(enabled: false, current: []).body, as: UTF8.self), "{\"capabilities\":[]}")
        for bad in ["Bad", "", "bad_token", "aé", String(repeating: "a", count: 33)] {
            XCTAssertThrowsError(try DeviceCapabilitiesUpdate(current: ["fed-serve"], adding: [bad])) { error in
                XCTAssertEqual((error as? DeviceApprovalError)?.field, "capabilities.\(bad)")
            }
        }
        XCTAssertThrowsError(try DeviceCapabilitiesUpdate(current: (0...8).map { "token-\($0)" }))
    }
}

private final class NoticeEvents: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [String] = []
    func add(_ event: String) { lock.lock(); defer { lock.unlock() }; values.append(event) }
    func read() -> [String] { lock.lock(); defer { lock.unlock() }; return values }
    func clear() { lock.lock(); defer { lock.unlock() }; values = [] }
}

extension DeviceApprovalNoticeTests {
    private func storeDirectory() throws -> URL {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
            .appendingPathComponent("../../target/scratch/notice-\(UUID().uuidString)").standardizedFileURL
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private func marks(_ directory: URL) throws -> [FedDeviceApprovalSeenMark] {
        let db = try FedSQLiteConnection.open(path: directory.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path, flags: SQLITE_OPEN_READONLY)
        defer { try? db.close() }
        let statement = try db.prepare("SELECT marks FROM device_approval_seen WHERE id = 1")
        guard try statement.step(), case .blob(let data) = statement.value(0) else { return [] }
        return try JSONDecoder().decode([FedDeviceApprovalSeenMark].self, from: data)
    }

    func testDurableSeenReplayResolutionHighestVersionAndPruning() async throws {
        let directory = try storeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let key = Data(repeating: 4, count: 32)
        let first = FedSQLiteStateStore(directoryURL: directory)
        _ = try await first.open(localPublicKey: key)
        let events = NoticeEvents()
        let pending = try notice()
        try await first.processDeviceApprovalNotice(pending, account: "account", nowMS: 2_000) { _ in events.add("pending") }
        try await first.close()
        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: key)
        try await reopened.processDeviceApprovalNotice(pending, account: "account", nowMS: 2_001) { _ in events.add("duplicate") }
        XCTAssertEqual(events.read(), ["pending"])
        XCTAssertEqual(try marks(directory).count, 1)
        let resolved = try notice(1)
        try await reopened.processDeviceApprovalNotice(resolved, account: "account", nowMS: 3_000) { _ in events.add("active") }
        XCTAssertEqual(try marks(directory).map(\.resolved), [true])
        try await reopened.processDeviceApprovalNotice(pending, account: "account", nowMS: 3_001) { _ in events.add("obsolete") }
        var payload = try object(object(rows()[1], "frame"), "payload").storage
        payload["state"] = .string("cancelled"); payload["state_version"] = .string("3")
        let cancelled = try DeviceApprovalNotice.decode(RdvJSONObject(payload))
        try await reopened.processDeviceApprovalNotice(cancelled, account: "account", nowMS: 4_000) { _ in events.add("cancelled") }
        try await reopened.processDeviceApprovalNotice(resolved, account: "account", nowMS: 4_001) { _ in events.add("old-active") }
        try await reopened.processDeviceApprovalNotice(cancelled, account: "account", nowMS: 4_002) { _ in events.add("duplicate-cancelled") }
        XCTAssertEqual(events.read(), ["pending", "active", "cancelled"])
        XCTAssertEqual(try marks(directory).map(\.version), [2, 3])
        try await reopened.pruneDeviceApprovalNotices(nowMS: 4_000 + FedDeviceApprovalSeenMark.retentionMS)
        XCTAssertTrue(try marks(directory).isEmpty)
        try await reopened.processDeviceApprovalNotice(pending, account: "account", nowMS: 2_000) { _ in events.add("pending-again") }
        try await reopened.pruneDeviceApprovalNotices(nowMS: pending.deadlineMS!)
        XCTAssertTrue(try marks(directory).isEmpty)
        try await reopened.processDeviceApprovalNotice(pending, account: "account", nowMS: pending.deadlineMS!) { _ in events.add("expired") }
        XCTAssertEqual(events.read().last, "pending-again")
        try await reopened.close()
    }

    func testPredecessorMismatchRefusesBeforeSigning() throws {
        let root = vectorDirectory.deletingLastPathComponent().appendingPathComponent("device-approval-phone-v1/phone.json")
        let fixture = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: root)) as? [String: Any])
        let valid = try XCTUnwrap(fixture["valid"] as? [[String: Any]])
        let row = try XCTUnwrap(valid.first { $0["name"] as? String == "approve-rendezvous-replacement" })
        let challengeJSON = try XCTUnwrap(row["challenge"] as? [String: Any])
        let challenge = try ActionChallenge(json: JSONSerialization.data(withJSONObject: challengeJSON))
        let original = try object(object(object(rows()[0], "frame"), "payload"), "enrollment")
        var view = try XCTUnwrap(JSONSerialization.jsonObject(with: bytes(original)) as? [String: Any])
        for (field, source) in ["account": "account", "enrollment_id": "enrollment_id", "state_version": "enrollment_version",
                                "roster": "target_roster", "device_id": "target_device_id", "fingerprint": "target_approval_fingerprint",
                                "ed25519_pubkey_hex": "ed25519_pubkey_hex", "x25519_pubkey_hex": "x25519_pubkey_hex", "predecessor": "predecessor"] {
            view[field] = challengeJSON[source]
        }
        let presented = try DeviceApprovalEnrollmentView(json: JSONSerialization.data(withJSONObject: view))
        XCTAssertNoThrow(try challenge.requirePresentedView(presented))
        view["predecessor"] = NSNull()
        let different = try DeviceApprovalEnrollmentView(json: JSONSerialization.data(withJSONObject: view))
        XCTAssertThrowsError(try DeviceApprovalActionSigner.sign(challenge, presentedView: different, keyData: Data(), reason: "Review security action")) { error in
            XCTAssertEqual((error as? DeviceApprovalError)?.code, .predecessorMismatch)
            XCTAssertEqual((error as? DeviceApprovalError)?.field, "predecessor")
        }
    }
}

private actor NoticeRecordingStream: FedWebSocketStream {
    let base: LoopbackWebSocketStream
    let events: NoticeEvents
    init(base: LoopbackWebSocketStream, events: NoticeEvents) { self.base = base; self.events = events }
    func send(_ message: FedWebSocketMessage) async throws {
        if case .text(let text) = message, text.contains("_ack\"") { events.add("ack") }
        try await base.send(message)
    }
    func receive() async throws -> FedWebSocketMessage? { try await base.receive() }
    func close() async { await base.close() }
}

extension DeviceApprovalNoticeTests {
    private func connected(store: FedSQLiteStateStore?, events: NoticeEvents) async throws -> (FedRendezvousClient, LoopbackWebSocketStream) {
        let pair = LoopbackWebSocketPair()
        let identity = try FedRendezvousIdentity(accountId: "01ARZ3NDEKTSV4RRFFQ69G5FAV", tokenId: "token",
            tokenVersion: "1", deviceToken: "test-device-token", x25519Key: FedNoiseKeyPair(privateKey: Data(repeating: 5, count: 32)),
            ed25519PrivateKey: Data(repeating: 6, count: 32))
        let signingKey = try RdvWireFixtures.signingKey()
        var configuration = FedRendezvousClient.Configuration(controlURL: URL(string: "wss://rdv.test.invalid/v1/ws")!, identity: identity,
            signingKeyPin: RdvAccountSigningKeyPin(keyId: signingKey.keyId, ed25519PublicKey: signingKey.publicKey))
        if let store {
            configuration.deviceApprovals = DeviceApprovalNoticeHandling(store: store, nowMS: { 2_000 }) { _ in events.add("present") }
        }
        let recording = NoticeRecordingStream(base: pair.client, events: events)
        let client = FedRendezvousClient(configuration: configuration) { _ in recording }
        let ephemeral = Curve25519.KeyAgreement.PrivateKey()
        try await pair.server.send(.text(RdvTestSigning.helloChallengeText(serverEphX25519PubkeyHex: ephemeral.publicKey.rawRepresentation.lowercaseHex)))
        try await pair.server.send(.text(RdvTestSigning.signedEnvelopeText(signPayload: RdvTestSigning.registrySnapshotPayload(serverSeq: "100", devices: []))))
        try await client.connect()
        _ = try await receive(pair.server)
        return (client, pair.server)
    }

    private func receive(_ server: LoopbackWebSocketStream) async throws -> String? {
        try await fedTestWithTimeout {
            guard case .text(let text)? = try await server.receive() else { return nil }
            return text
        }
    }

    func testControlConnectionPresentsThenCommitsThenAcksEveryDelivery() async throws {
        let directory = try storeDirectory()
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = FedSQLiteStateStore(directoryURL: directory)
        _ = try await store.open(localPublicKey: Data(repeating: 5, count: 32))
        let events = NoticeEvents()
        let (client, server) = try await connected(store: store, events: events)
        await store.setCommitBarrier { events.add($0.rawValue) }
        let payload = try object(object(rows()[0], "frame"), "payload")
        let text = try RdvTestSigning.signedEnvelopeText(signPayload: payload)
        var tampered = payload.storage; tampered["server_seq"] = .string("102")
        try await server.send(.text(RdvTestSigning.signedEnvelopeText(signPayload: payload, wirePayload: RdvJSONObject(tampered))))
        try await server.send(.text(text))
        let receivedAck = try await receive(server)
        let ackText = try XCTUnwrap(receivedAck)
        let ack = try DeviceApprovalNoticeAck.decode(RdvJSONValue.parseObject(Data(ackText.utf8)))
        XCTAssertEqual(ack.seq, "2")
        XCTAssertEqual(ack.noticeID, try notice().noticeID)
        XCTAssertEqual(events.read(), ["present", "beforeCommit", "afterCommit", "ack"])
        let invalidSignatures = await client.invalidSignatureCount
        XCTAssertEqual(invalidSignatures, 1)
        XCTAssertEqual(try marks(directory).count, 1)
        events.clear()
        try await server.send(.text(text))
        let receivedRepeated = try await receive(server)
        let repeatedText = try XCTUnwrap(receivedRepeated)
        let repeated = try DeviceApprovalNoticeAck.decode(RdvJSONValue.parseObject(Data(repeatedText.utf8)))
        XCTAssertEqual(repeated.seq, "3")
        XCTAssertEqual(events.read(), ["ack"])
        events.clear()
        var replay = payload.storage; replay["server_seq"] = .string("102")
        try await server.send(.text(RdvTestSigning.signedEnvelopeText(signPayload: RdvJSONObject(replay))))
        _ = try await receive(server)
        XCTAssertEqual(events.read(), ["beforeCommit", "afterCommit", "ack"])
        let gapCount = await client.gapCount
        XCTAssertEqual(gapCount, 0, "duplicate delivery must not advance the cursor")
        await client.disconnect()
        try await store.close()
    }

    func testCommitFailuresNeverAckAndCrashAfterCommitDoesNotLoseSeenMark() async throws {
        for barrier in [FedSQLiteStateStore.CommitBarrier.beforeCommit, .afterCommit] {
            let directory = try storeDirectory()
            defer { try? FileManager.default.removeItem(at: directory) }
            let store = FedSQLiteStateStore(directoryURL: directory)
            let key = Data(repeating: 5, count: 32)
            _ = try await store.open(localPublicKey: key)
            let events = NoticeEvents()
            let (client, server) = try await connected(store: store, events: events)
            await store.setCommitBarrier { point in
                events.add(point.rawValue)
                if point == barrier { throw FedFailure.persistenceFailed }
            }
            let payload = try object(object(rows()[0], "frame"), "payload")
            try await server.send(.text(RdvTestSigning.signedEnvelopeText(signPayload: payload)))
            let reply = try await receive(server)
            XCTAssertNil(reply, "a commit failure must close without acknowledging")
            XCTAssertFalse(events.read().contains("ack"))
            XCTAssertEqual(try marks(directory).count, barrier == .afterCommit ? 1 : 0)
            await client.disconnect()
            try await store.close()
            let reopened = FedSQLiteStateStore(directoryURL: directory)
            _ = try await reopened.open(localPublicKey: key)
            try await reopened.processDeviceApprovalNotice(notice(), account: "01ARZ3NDEKTSV4RRFFQ69G5FAV", nowMS: 2_001) { _ in events.add("replay") }
            XCTAssertEqual(events.read().contains("replay"), barrier == .beforeCommit)
            try await reopened.close()
        }
    }

    func testClientWithoutConfiguredApprovalsKeepsSessionAndCursorWithoutAcknowledging() async throws {
        let events = NoticeEvents()
        let (client, server) = try await connected(store: nil, events: events)
        let payload = try object(object(rows()[0], "frame"), "payload")
        try await server.send(.text(RdvTestSigning.signedEnvelopeText(signPayload: payload)))
        let refusal = RdvJSONObject([
            "type": .string("refusal"), "server_seq": .string("102"), "of_type": .string("relay_open"),
            "of_seq": .string("2"), "code": .string("peer_unreachable"), "message": .string("unreachable"),
        ])
        try await server.send(.text(RdvCanonicalJSON.canonicalString(.object(refusal))))
        try await fedTestWithTimeout {
            while await client.lastRefusal?.serverSeq != "102" {
                try await Task.sleep(nanoseconds: 2_000_000)
            }
        }
        let ready = await client.isReady
        let resyncs = await client.resyncCount
        let gaps = await client.gapCount
        let unhandled = await client.unhandledDeviceApprovalNoticeCount
        let outgoing = await server.tryReceiveNow()
        XCTAssertTrue(ready)
        XCTAssertEqual(resyncs, 0)
        XCTAssertEqual(gaps, 0)
        XCTAssertEqual(unhandled, 1)
        XCTAssertNil(outgoing)
        XCTAssertTrue(events.read().isEmpty)
        await client.disconnect()
    }
}
