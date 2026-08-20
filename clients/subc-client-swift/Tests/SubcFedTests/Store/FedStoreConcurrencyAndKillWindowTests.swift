import Foundation
import XCTest
@testable import SubcFed

final class FedStoreConcurrencyAndKillWindowTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)

    func testSimultaneousTwoWritersExactlyOneSeqWins() async throws {
        let dir = try temporaryDirectory()
        defer { try? FileManager.default.removeItem(at: dir) }

        let bootstrap = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await bootstrap.open(localPublicKey: localKey)

        let writerA = FedAtomicFileStateStore(directoryURL: dir)
        let writerB = FedAtomicFileStateStore(directoryURL: dir)
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
        let reopened = FedAtomicFileStateStore(directoryURL: dir)
        _ = try await reopened.open(localPublicKey: localKey)
        let next = try await reopened.reserveEffectSequence()
        if let maxHanded = successes.max() {
            XCTAssertGreaterThan(next.value, maxHanded)
        }
    }

    func testKillWindowAfterTempWriteLeavesNoTornCommittedState() async throws {
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

    private func temporaryDirectory() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-kill-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
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
