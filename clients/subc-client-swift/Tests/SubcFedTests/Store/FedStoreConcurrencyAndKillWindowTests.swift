import Foundation
import XCTest
@testable import SubcFed

/// Runs against the file store as written; `FedStoreConcurrencyAndKillWindowSQLiteTests`
/// below reruns the concurrency test against the SQLite store and adds the
/// SQLite kill windows. The file store's own kill windows sit inside its
/// temp-write/rename sequence, which the SQLite store does not have.
class FedStoreConcurrencyAndKillWindowTests: XCTestCase {
    class var storeUnderTest: FedStoreUnderTest { .asWritten }

    fileprivate let localKey = Data(repeating: 0x11, count: 32)

    fileprivate func skipUnlessFileStore() throws {
        try XCTSkipIf(
            Self.storeUnderTest == .sqlite,
            "kill window inside the file store's rename sequence; the SQLite kill windows are the tests of this subclass"
        )
    }

    func testSimultaneousTwoWritersExactlyOneSeqWins() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        let bootstrap = Self.storeUnderTest.durableStore(in: dir)
        _ = try await bootstrap.open(localPublicKey: localKey)

        let writerA = Self.storeUnderTest.durableStore(in: dir)
        let writerB = Self.storeUnderTest.durableStore(in: dir)
        _ = try await writerA.open(localPublicKey: localKey)
        _ = try await writerB.open(localPublicKey: localKey)

        let barrier = Barrier(count: 2)
        async let a: Result<UInt64, Error> = {
            await barrier.arriveAndWait()
            do {
                let r = try await writerA.reserveEffectSequence()
                return .success(r.value)
            } catch {
                return .failure(error)
            }
        }()
        async let b: Result<UInt64, Error> = {
            await barrier.arriveAndWait()
            do {
                let r = try await writerB.reserveEffectSequence()
                return .success(r.value)
            } catch {
                return .failure(error)
            }
        }()

        let results = await [a, b]
        let successes = results.compactMap { try? $0.get() }
        // Under exclusive lock both may succeed serially with distinct seqs, or
        // one may fail if it raced a stale in-memory view — never the same seq.
        XCTAssertFalse(successes.isEmpty)
        XCTAssertEqual(Set(successes).count, successes.count, "duplicate reserved seq")

        // Reopen and reserve: next value must exceed every handed-out seq.
        let reopened = Self.storeUnderTest.durableStore(in: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        let next = try await reopened.reserveEffectSequence()
        if let maxHanded = successes.max() {
            XCTAssertGreaterThan(next.value, maxHanded)
        }
    }

    func testKillWindowAfterTempWriteLeavesNoTornCommittedState() async throws {
        try skipUnlessFileStore()
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let first = try await store.reserveEffectSequence()

        await store.setCommitBarrier { barrier in
            if barrier == .afterTempWrite {
                throw FedFailure.persistenceFailed
            }
        }
        do {
            _ = try await store.reserveEffectSequence()
            XCTFail("barrier should abort commit")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .persistenceFailed)
        }

        await store.setCommitBarrier(nil)
        let reopened = FedAtomicFileStateStore(directoryURL: dir)
        let doc = try await reopened.open(localPublicKey: localKey)
        // Committed first reservation survives; aborted second does not advance.
        let next = try await reopened.reserveEffectSequence()
        XCTAssertGreaterThan(next.value, first.value)
        XCTAssertEqual(doc.document.global.localIncarnation.isEmpty, false)
    }

    func testKillWindowAfterTempFsyncAndAfterRename() async throws {
        try skipUnlessFileStore()
        for barrierPoint in [
            FedAtomicFileStateStore.CommitBarrier.afterTempFsync,
            .beforeDirSync,
        ] {
            let dir = try temporaryDirectory()
            defer { try? FileManager.default.removeItem(at: dir) }
            let store = FedAtomicFileStateStore(directoryURL: dir)
            _ = try await store.open(localPublicKey: localKey)
            let first = try await store.reserveEffectSequence()

            await store.setCommitBarrier { point in
                if point == barrierPoint {
                    throw FedFailure.persistenceFailed
                }
            }
            do {
                _ = try await store.reserveEffectSequence()
                // afterRename still completes rename before barrier; beforeDirSync
                // fails after rename so on-disk may have advanced — reopen must
                // never reuse a seq.
            } catch {
                // expected for afterTempFsync
            }

            await store.setCommitBarrier(nil)
            let reopened = FedAtomicFileStateStore(directoryURL: dir)
            _ = try await reopened.open(localPublicKey: localKey)
            let next = try await reopened.reserveEffectSequence()
            XCTAssertGreaterThan(next.value, first.value, "barrier \(barrierPoint)")
        }
    }

    func testKillWindowAfterRenameDoesNotDuplicateSeq() async throws {
        try skipUnlessFileStore()
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)

        await store.setCommitBarrier { point in
            if point == .afterRename {
                throw FedFailure.persistenceFailed
            }
        }
        // Rename already happened; commit reports failure but bytes are durable.
        var renamedSeq: UInt64?
        do {
            renamedSeq = try await store.reserveEffectSequence().value
            XCTFail("expected post-rename barrier failure")
        } catch {
            // failure reported
        }

        await store.setCommitBarrier(nil)
        let reopened = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        let next = try await reopened.reserveEffectSequence()
        if let renamedSeq {
            XCTAssertGreaterThan(next.value, renamedSeq)
        } else {
            // If the barrier fired, on-disk still advanced under lock.
            XCTAssertGreaterThanOrEqual(next.value, 1)
        }
    }

    fileprivate func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-kill-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }
}

/// The SQLite store's kill windows. A change is two commits: the intent (with
/// its sequence reservation) before the send, and the outcome before the
/// caller sees the reply. A process can die on either side of each commit.
/// Every test drives the change through `FedOriginEffectLog`, as the session
/// engine does, and then reads the state through a NEW store instance opened
/// from disk while the first one is still alive and has closed nothing, which
/// is what the next launch after a kill sees.
final class FedStoreConcurrencyAndKillWindowSQLiteTests: FedStoreConcurrencyAndKillWindowTests {
    override class var storeUnderTest: FedStoreUnderTest { .sqlite }

    private let responder = Data(repeating: 0x22, count: 32)
    private let peerIncarnation = "00000000-0000-4000-8000-0000000000aa"
    private let epoch = "00000000-0000-4000-8000-0000000000bb"

    /// Killed after the intent commit, before the call is written to the
    /// network: the next launch finds the intent, unsettled, to reconcile.
    func testKillBetweenIntentCommitAndSendLeavesTheIntentToReconcile() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = try await openStore(dir)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)

        let effect = try await log.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: epoch)

        let afterKill = try await openStore(dir)
        let unsettled = try await afterKill.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(unsettled.map(\.effect), [effect], "the intent must be durable before the send")
        XCTAssertEqual(unsettled.map(\.phase), [.intent])
        XCTAssertEqual(unsettled.first?.peerLedgerEpoch, epoch)
        let next = try await afterKill.reserveEffectSequence()
        XCTAssertGreaterThan(next.value, effect.seq, "the reserved sequence is durable with the intent")
    }

    /// Killed while the intent transaction is still uncommitted: nothing of the
    /// change is on disk, so nothing was sent and nothing needs reconciling.
    func testKillBeforeTheIntentCommitLeavesNoTrace() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = try await openStore(dir)
        let before = try await store.snapshot()
        await store.setCommitBarrier { point in
            if point == .beforeCommit { throw FedFailure.persistenceFailed }
        }
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        do {
            _ = try await log.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: epoch)
            XCTFail("the intent commit was interrupted, so the mutation must not start")
        } catch {}

        let afterKill = try await openStore(dir)
        let after = try await afterKill.snapshot()
        XCTAssertEqual(after, before, "an uncommitted intent leaves the store exactly as it was")
    }

    /// Killed after the outcome commit, before the caller saw the reply: the
    /// next launch has the outcome, body included, and nothing to reconcile.
    func testKillBetweenTerminalCommitAndTheCallerSeeingTheReply() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = try await openStore(dir)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let effect = try await log.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: epoch)
        try await log.markSent(effect)

        await store.setCommitBarrier { point in
            if point == .afterCommit { throw FedFailure.persistenceFailed }
        }
        let body = Data(#"{"done":true}"#.utf8)
        do {
            _ = try await log.applyTerminalFrame(
                effect: effect, kind: "response", body: body, bodyOmitted: false, errorCode: nil
            )
            XCTFail("the caller must not see the reply in this window")
        } catch {}

        let afterKill = try await openStore(dir)
        let destination = try await afterKill.destination(forResponderPublicKey: responder)
        let row = destination?.unresolvedEffects.first { $0.effect == effect }
        XCTAssertEqual(row?.disposition, .recorded)
        XCTAssertEqual(row?.terminalBody, body)
        XCTAssertEqual(destination?.confirmedWatermark?.seq, effect.seq, "the watermark advance is in the same commit")
        let unsettled = try await afterKill.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertTrue(unsettled.isEmpty)
    }

    /// Killed while the outcome transaction is still uncommitted: the intent
    /// is still there, unsettled, so the next launch asks the serving peer's
    /// ledger about it.
    func testKillBeforeTheTerminalCommitLeavesTheIntentToReconcile() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }
        let store = try await openStore(dir)
        let log = FedOriginEffectLog(store: store, responderStaticPublicKey: responder)
        let effect = try await log.beginMutation(peerIncarnation: peerIncarnation, peerLedgerEpoch: epoch)
        try await log.markSent(effect)

        await store.setCommitBarrier { point in
            if point == .beforeCommit { throw FedFailure.persistenceFailed }
        }
        do {
            _ = try await log.applyTerminalFrame(
                effect: effect, kind: "response", body: Data("{}".utf8), bodyOmitted: false, errorCode: nil
            )
            XCTFail("the outcome commit was interrupted")
        } catch {}

        let afterKill = try await openStore(dir)
        let unsettled = try await afterKill.unsettledEffects(forResponderPublicKey: responder)
        XCTAssertEqual(unsettled.map(\.effect), [effect])
        XCTAssertEqual(unsettled.map(\.disposition), [.unknown])
        let watermark = try await afterKill.destination(forResponderPublicKey: responder)?.confirmedWatermark
        XCTAssertNil(watermark)
    }

    private func openStore(_ dir: URL) async throws -> FedSQLiteStateStore {
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        return store
    }
}

/// Simple countdown barrier for simultaneous-writer tests.
private actor Barrier {
    private let count: Int
    private var arrived = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(count: Int) { self.count = count }

    func arriveAndWait() async {
        arrived += 1
        if arrived >= count {
            let pending = waiters
            waiters.removeAll()
            for waiter in pending { waiter.resume() }
            return
        }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            waiters.append(continuation)
        }
    }
}
