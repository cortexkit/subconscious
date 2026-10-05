import Foundation
import SQLite3
import XCTest
@testable import SubcFed

final class FedPeerMachineIDStoreTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)
    private let otherResponder = Data(repeating: 0x33, count: 32)
    private let firstID = "853ecdb6e1681c3dc8c1c578d14c23a2"
    private let secondID = "00000000000000000000000000000002"

    func testRecordsMachineIDPerResponderKeyAndAcrossReopen() async throws {
        let (store, directory) = try await openStore()
        let absent = try await store.peerMachineID(forResponderPublicKey: responder)
        XCTAssertNil(absent)
        try await observe(store, key: responder, id: firstID)
        try await observe(store, key: otherResponder, id: secondID)
        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: localKey)
        let first = try await reopened.peerMachineID(forResponderPublicKey: responder)
        let second = try await reopened.peerMachineID(forResponderPublicKey: otherResponder)
        XCTAssertEqual(first, firstID)
        XCTAssertEqual(second, secondID)
    }

    func testOmittedMachineIDKeepsTheStoredName() async throws {
        let (store, directory) = try await openStore()
        try await observe(store, key: responder, id: firstID)
        try await observe(store, key: responder, id: nil)
        // Existing callers of the old overload omit a name too.
        try await store.observePeerHello(
            responderStaticPublicKey: responder, peerIncarnation: "new-incarnation", peerLedgerEpoch: "new-epoch"
        )
        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: localKey)
        let saved = try await reopened.peerMachineID(forResponderPublicKey: responder)
        XCTAssertEqual(saved, firstID)
    }

    func testChangedMachineIDOverwritesAndLogsWithoutChangingRecoveryState() async throws {
        let (store, _) = try await openStore()
        let messages = MachineIDLogMessages()
        await store.setMachineIDChangeLogger { messages.append($0) }
        try await observe(store, key: responder, id: firstID)
        let before = try await store.destination(forResponderPublicKey: responder)
        try await observe(store, key: responder, id: firstID)
        try await observe(store, key: responder, id: nil)
        XCTAssertEqual(messages.snapshot(), [])
        try await observe(store, key: responder, id: secondID)
        let saved = try await store.peerMachineID(forResponderPublicKey: responder)
        let after = try await store.destination(forResponderPublicKey: responder)
        XCTAssertEqual(saved, secondID)
        XCTAssertEqual(after, before)
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responder)
        XCTAssertEqual(messages.snapshot(), [
            "Peer \(fp) changed machine_id from \(firstID) to \(secondID); responder pin unchanged",
        ])
    }

    func testUnpinCleanupRemovesOnlyMachineIDAndPreservesEffects() async throws {
        let (store, directory) = try await openStore()
        try await observe(store, key: responder, id: firstID)
        try await observe(store, key: otherResponder, id: secondID)
        _ = try await store.reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responder, peerLedgerEpoch: "epoch", peerIncarnation: "peer"
        )
        let before = try await store.destination(forResponderPublicKey: responder)
        XCTAssertEqual(before?.unresolvedEffects.count, 1)
        try await store.forgetPeerMachineID(forResponderPublicKey: responder)
        // The app may repeat unpin cleanup; it is harmless.
        try await store.forgetPeerMachineID(forResponderPublicKey: responder)
        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: localKey)
        let removed = try await reopened.peerMachineID(forResponderPublicKey: responder)
        let other = try await reopened.peerMachineID(forResponderPublicKey: otherResponder)
        let after = try await reopened.destination(forResponderPublicKey: responder)
        XCTAssertNil(removed)
        XCTAssertEqual(other, secondID)
        XCTAssertEqual(after, before)
    }

    func testLayoutOneMigrationPreservesExistingRowsAndAddsMachineIDStorage() async throws {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let fp = FedStateDocument.destinationKey(forResponderPublicKey: responder)
        let effect = FedUnresolvedEffectRecord(
            effect: FedEffectID(incarnation: "00000000-0000-4000-8000-000000000001", seq: 7),
            responderStaticPublicKey: responder, peerLedgerEpoch: "epoch"
        )
        let document = FedStateDocument(
            localIdentityDigest: FedStateDocument.identityDigest(forPublicKey: localKey),
            localPublicKey: localKey, revision: 9, global: .mintFresh(),
            destinations: [fp: FedDestinationState(
                responderStaticPublicKey: responder, observedPeerIncarnation: "peer",
                observedPeerLedgerEpoch: "epoch", unresolvedEffects: [effect]
            )]
        )
        let db = try FedSQLiteConnection.open(
            path: directory.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path,
            flags: FedSQLiteStateStore.openFlags | SQLITE_OPEN_CREATE
        )
        // Frozen layout 1 from before machine-name storage, including its
        // additive confirmed-range table. Do not seed with the current schema:
        // doing so would test fresh creation instead of migration.
        try db.execute("""
            BEGIN IMMEDIATE;
            CREATE TABLE meta(key TEXT PRIMARY KEY NOT NULL, value);
            CREATE TABLE destination(responder_fp TEXT PRIMARY KEY NOT NULL, responder_pubkey BLOB NOT NULL,
                observed_peer_incarnation TEXT, observed_peer_ledger_epoch TEXT, confirmed_incarnation TEXT, confirmed_seq INTEGER);
            CREATE TABLE effect(responder_fp TEXT NOT NULL, incarnation TEXT NOT NULL, seq INTEGER NOT NULL,
                phase TEXT NOT NULL, disposition TEXT NOT NULL, peer_ledger_epoch TEXT, peer_incarnation TEXT,
                terminal_kind TEXT, terminal_code TEXT, terminal_body BLOB, PRIMARY KEY (responder_fp, incarnation, seq));
            CREATE INDEX effect_by_disposition ON effect(responder_fp, disposition);
            CREATE TABLE poisoned_epoch(responder_fp TEXT NOT NULL, epoch TEXT NOT NULL, PRIMARY KEY (responder_fp, epoch));
            CREATE TABLE confirmed_range(responder_fp TEXT NOT NULL, incarnation TEXT NOT NULL, from_seq INTEGER NOT NULL,
                to_seq INTEGER NOT NULL, PRIMARY KEY (responder_fp, incarnation, from_seq));
            """)
        try FedSQLiteStoreRows.insert(document, into: db)
        try db.execute("UPDATE meta SET value = 1 WHERE key = 'schema_version'; COMMIT;")
        let missingTable = try db.prepare("SELECT name FROM sqlite_master WHERE name = 'peer_machine_id'")
        XCTAssertFalse(try missingTable.step(), "control: the old store has no machine-name table")
        let store = FedSQLiteStateStore(directoryURL: directory)
        let opened = try await store.open(localPublicKey: localKey)
        XCTAssertFalse(opened.created)
        XCTAssertEqual(opened.document, document)
        let initialName = try await store.peerMachineID(forResponderPublicKey: responder)
        XCTAssertNil(initialName)
        try await observe(store, key: responder, id: firstID)
        let saved = try await store.peerMachineID(forResponderPublicKey: responder)
        XCTAssertEqual(saved, firstID)
        let reopened = FedSQLiteStateStore(directoryURL: directory)
        _ = try await reopened.open(localPublicKey: localKey)
        let persisted = try await reopened.peerMachineID(forResponderPublicKey: responder)
        let destination = try await reopened.destination(forResponderPublicKey: responder)
        XCTAssertEqual(persisted, firstID)
        XCTAssertEqual(destination?.unresolvedEffects, [effect])
    }

    private func openStore() async throws -> (FedSQLiteStateStore, URL) {
        let directory = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: directory)
        _ = try await store.open(localPublicKey: localKey)
        return (store, directory)
    }

    private func observe(_ store: FedSQLiteStateStore, key: Data, id: String?) async throws {
        try await store.observePeerHello(
            responderStaticPublicKey: key, peerIncarnation: "peer", peerLedgerEpoch: "epoch", peerMachineID: id
        )
    }
}

private final class MachineIDLogMessages: @unchecked Sendable {
    private let lock = NSLock()
    private var messages: [String] = []
    func append(_ message: String) {
        lock.lock()
        defer { lock.unlock() }
        messages.append(message)
    }
    func snapshot() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        return messages
    }
}
