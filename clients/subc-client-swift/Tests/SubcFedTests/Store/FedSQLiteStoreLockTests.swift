import Foundation
import XCTest
@testable import SubcFed
#if os(macOS)
import Darwin
#endif

final class FedSQLiteStoreLockTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let responder = Data(repeating: 0x22, count: 32)

    func testRefusedOpenerNeverCreatesDatabase() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let lock = try FedSQLiteStoreLock(url: dir.appendingPathComponent("fed-state.lock"))
        let store = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await store.open(localPublicKey: localKey)
            XCTFail("ownership must be acquired before database creation")
        } catch is FedStateStoreInUse {}
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path))
        withExtendedLifetime(lock) {}
    }

    func testSameProcessSecondOpenerIsRefusedBeforeDatabaseWork() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let first = FedSQLiteStateStore(directoryURL: dir)
        _ = try await first.open(localPublicKey: localKey)
        let legacy = dir.appendingPathComponent("fed-state.json")
        let building = dir.appendingPathComponent(FedSQLiteStateStore.buildingFileName)
        try Data("leave legacy alone".utf8).write(to: legacy)
        try Data("leave build alone".utf8).write(to: building)
        let database = dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName)
        let before = try Data(contentsOf: database)
        let second = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await second.open(localPublicKey: responder)
            XCTFail("a second opener must fail before even checking the identity")
        } catch is FedStateStoreInUse {}
        XCTAssertEqual(try Data(contentsOf: database), before)
        XCTAssertTrue(FileManager.default.fileExists(atPath: legacy.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: building.path))
        try await assertClosed { _ = try await second.snapshot() }
        try await first.close()
        _ = try await second.open(localPublicKey: localKey)
        try await second.close()
    }

    func testNewOwnerReloadsSequenceDestinationAndDiscardsTransientSentState() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let first = FedSQLiteStateStore(directoryURL: dir)
        let original = try await first.open(localPublicKey: localKey)
        let effect = try await first.reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responder, peerLedgerEpoch: "epoch", peerIncarnation: "peer"
        )
        try await first.markSent(effect: effect, responderStaticPublicKey: responder)
        try await first.close()
        let second = FedSQLiteStateStore(directoryURL: dir)
        _ = try await second.open(localPublicKey: localKey)
        let phases = try await second.unsettledEffects(forResponderPublicKey: responder).map(\.phase)
        XCTAssertEqual(phases, [.intent], "close must not persist a transient sent phase")
        let reserved = try await second.reserveEffectSequence()
        try await second.observePeerHello(
            responderStaticPublicKey: responder, peerIncarnation: "new-peer",
            peerLedgerEpoch: "new-epoch", peerMachineID: "new-machine"
        )
        let expected = try await second.snapshot()
        try await second.close()
        let returning = FedSQLiteStateStore(directoryURL: dir)
        let reopened = try await returning.open(localPublicKey: localKey)
        XCTAssertFalse(reopened.created)
        XCTAssertEqual(reopened.document, expected)
        XCTAssertEqual(reopened.document.global.localIncarnation, original.document.global.localIncarnation)
        let next = try await returning.reserveEffectSequence()
        XCTAssertGreaterThan(next.value, reserved.value)
        let machine = try await returning.peerMachineID(forResponderPublicKey: responder)
        XCTAssertEqual(machine, "new-machine")
        try await returning.close()
    }

    func testEveryStoreOperationFailsAfterCloseWithoutReopening() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: dir)
        let opened = try await store.open(localPublicKey: localKey)
        let effect = FedEffectID(incarnation: opened.document.global.localIncarnation, seq: 1)
        try await store.close()
        try await store.close()
        let other = FedSQLiteStateStore(directoryURL: dir)
        _ = try await other.open(localPublicKey: localKey)
        let before = try await other.snapshot()
        try await assertClosed { _ = try await store.open(localPublicKey: self.localKey) }
        try await assertClosed { _ = try await store.snapshot() }
        try await assertClosed { _ = try await store.reserveCatalogGeneration() }
        try await assertClosed { _ = try await store.reserveEffectSequence() }
        try await assertClosed {
            _ = try await store.reserveEffectSequenceAndCommitIntent(
                responderStaticPublicKey: self.responder, peerLedgerEpoch: nil, peerIncarnation: nil
            )
        }
        try await assertClosed {
            try await store.commitIntent(FedUnresolvedEffectRecord(effect: effect, responderStaticPublicKey: self.responder))
        }
        try await assertClosed { try await store.markSent(effect: effect, responderStaticPublicKey: self.responder) }
        try await assertClosed {
            try await store.commitTerminal(
                effect: effect, responderStaticPublicKey: self.responder, disposition: .unknown,
                terminalBody: nil, terminalKind: nil, terminalCode: nil
            )
        }
        try await assertClosed {
            try await store.commitConfirmedWatermark(
                responderStaticPublicKey: self.responder,
                watermark: FedConfirmedWatermark(incarnation: effect.incarnation, seq: 1)
            )
        }
        try await assertClosed {
            try await store.observePeerHello(
                responderStaticPublicKey: self.responder, peerIncarnation: "peer", peerLedgerEpoch: "epoch"
            )
        }
        try await assertClosed {
            try await store.observePeerHello(
                responderStaticPublicKey: self.responder, peerIncarnation: "peer",
                peerLedgerEpoch: "epoch", peerMachineID: "machine"
            )
        }
        try await assertClosed { _ = try await store.peerMachineID(forResponderPublicKey: self.responder) }
        try await assertClosed { try await store.forgetPeerMachineID(forResponderPublicKey: self.responder) }
        try await assertClosed { try await store.poisonLedgerEpoch(responderStaticPublicKey: self.responder, epoch: "epoch") }
        try await assertClosed { try await store.acknowledgeReenrollment(FedReenrollmentAcknowledgment(enrollmentID: "e", atMs: 1)) }
        try await assertClosed { _ = try await store.destination(forResponderPublicKey: self.responder) }
        try await assertClosed { _ = try await store.unsettledEffects(forResponderPublicKey: self.responder) }
        let after = try await other.snapshot()
        XCTAssertEqual(after, before)
        try await other.close()
    }

    func testDeinitReleasesOwnership() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        var store: FedSQLiteStateStore? = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store!.open(localPublicKey: localKey)
        let reservation = try await store!.reserveEffectSequence()
        store = nil
        let next = FedSQLiteStateStore(directoryURL: dir)
        _ = try await next.open(localPublicKey: localKey)
        let value = try await next.reserveEffectSequence()
        XCTAssertGreaterThan(value.value, reservation.value)
        try await next.close()
    }

    func testEstablishWithClosedActorFailsBeforeSendingOrReopening() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let closed = FedSQLiteStateStore(directoryURL: dir)
        _ = try await closed.open(localPublicKey: localKey)
        try await closed.close()
        let owner = FedSQLiteStateStore(directoryURL: dir)
        _ = try await owner.open(localPublicKey: localKey)
        let transport = FedLoopbackByteTransport()
        let engine = FedSessionEngine(deps: .init(
            transport: transport, store: closed, clock: FedFakeClock(),
            localPublicKey: localKey, responderStaticPublicKey: responder,
            helloPolicy: try FedHelloPolicy(), connectionAttemptID: String(repeating: "c", count: 32)
        ))
        try await assertClosed { try await engine.establish() }
        let frames = try await transport.sentFrames(negotiationComplete: false)
        XCTAssertTrue(frames.isEmpty)
        await engine.disconnect()
        try await owner.close()
    }

    func testLockFileIsOwnerOnlyAndSymlinksAreRefused() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let lock = dir.appendingPathComponent("fed-state.lock")
        let attributes = try FileManager.default.attributesOfItem(atPath: lock.path)
        XCTAssertEqual((attributes[.posixPermissions] as? NSNumber)?.intValue, 0o600)
        try await store.close()
        try FileManager.default.removeItem(at: lock)
        let target = dir.appendingPathComponent("not-a-lock")
        try Data("unchanged".utf8).write(to: target)
        try FileManager.default.createSymbolicLink(at: lock, withDestinationURL: target)
        let next = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await next.open(localPublicKey: localKey)
            XCTFail("must not follow a lock-file symlink")
        } catch let error as FedFailure {
            XCTAssertEqual(error, .storeUnavailable)
        }
        XCTAssertEqual(try Data(contentsOf: target), Data("unchanged".utf8))
    }

    private func assertClosed(_ operation: () async throws -> Void, line: UInt = #line) async throws {
        do {
            try await operation()
            XCTFail("a closed store must not reopen itself", line: line)
        } catch is FedStateStoreClosed {}
    }

    #if os(macOS)
    func testCloseLeavesNoOpenDescriptorsInStoreDirectory() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        let descriptors = try descriptors(in: dir)
        let lock = try XCTUnwrap(descriptors.first { $0.path.hasSuffix("/fed-state.lock") })
        XCTAssertNotEqual(fcntl(lock.fd, F_GETFD) & FD_CLOEXEC, 0)
        XCTAssertTrue(descriptors.contains { $0.path.hasSuffix("/fed-state.sqlite") })
        try await store.close()
        XCTAssertTrue(try self.descriptors(in: dir).isEmpty, "close left a file descriptor in the App Group directory")
    }

    private func descriptors(in directory: URL) throws -> [(fd: Int32, path: String)] {
        let prefix = directory.resolvingSymlinksInPath().path + "/"
        return try FileManager.default.contentsOfDirectory(atPath: "/dev/fd").compactMap { name in
            guard let fd = Int32(name) else { return nil }
            var path = [CChar](repeating: 0, count: Int(MAXPATHLEN))
            guard fcntl(fd, F_GETPATH, &path) == 0 else { return nil }
            let resolved = URL(fileURLWithPath: String(cString: path)).resolvingSymlinksInPath().path
            return resolved.hasPrefix(prefix) ? (fd, resolved) : nil
        }
    }

    func testOtherProcessOwnershipAndWritesAfterClose() async throws {
        try await checkOtherProcess(releaseByExit: false)
    }

    func testOtherProcessOwnershipAndWritesAfterExit() async throws {
        try await checkOtherProcess(releaseByExit: true)
    }

    /// Re-run only this test in a separate XCTest process. Readiness is written
    /// after the durable writes, and release is signalled by the parent test.
    func testLockHolderHelper() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let path = environment["SUBCFED_LOCK_HELPER_DIRECTORY"] else { return }
        let dir = URL(fileURLWithPath: path, isDirectory: true)
        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        _ = try await store.reserveEffectSequenceAndCommitIntent(
            responderStaticPublicKey: responder, peerLedgerEpoch: "helper-epoch", peerIncarnation: "helper-peer"
        )
        try await store.observePeerHello(
            responderStaticPublicKey: responder, peerIncarnation: "helper-peer",
            peerLedgerEpoch: "helper-epoch", peerMachineID: "helper-machine"
        )
        let expected = try await store.snapshot()
        try JSONEncoder().encode(expected).write(to: dir.appendingPathComponent("helper-ready"), options: .atomic)
        let release = dir.appendingPathComponent("helper-release")
        let deadline = Date().addingTimeInterval(20)
        while !FileManager.default.fileExists(atPath: release.path) {
            guard Date() < deadline else {
                try await store.close()
                XCTFail("parent never released lock holder")
                return
            }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        if environment["SUBCFED_LOCK_HELPER_EXIT"] == "1" {
            // Bypass Swift deinit to prove the kernel releases ownership on exit.
            _exit(0)
        }
        try await store.close()
        // Stay alive until the parent has reopened, so close, not process exit,
        // is what permits the new owner to acquire the lock.
        let finished = dir.appendingPathComponent("parent-reopened")
        while !FileManager.default.fileExists(atPath: finished.path) {
            guard Date() < deadline else { XCTFail("parent never reopened"); return }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
    }

    private func checkOtherProcess(releaseByExit: Bool) async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let child = Process()
        child.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        child.arguments = [
            "xctest", "-XCTest", "SubcFedTests.FedSQLiteStoreLockTests/testLockHolderHelper",
            Bundle(for: FedSQLiteStoreLockTests.self).bundleURL.path,
        ]
        var environment = ProcessInfo.processInfo.environment
        environment["SUBCFED_LOCK_HELPER_DIRECTORY"] = dir.path
        environment["SUBCFED_LOCK_HELPER_EXIT"] = releaseByExit ? "1" : "0"
        child.environment = environment
        // A file avoids pipe backpressure if XCTest emits verbose diagnostics.
        let output = dir.appendingPathComponent("helper-output")
        XCTAssertTrue(FileManager.default.createFile(atPath: output.path, contents: nil))
        let handle = try FileHandle(forWritingTo: output)
        defer { try? handle.close() }
        child.standardOutput = handle
        child.standardError = handle
        try child.run()
        defer {
            if child.isRunning { child.terminate() }
            child.waitUntilExit()
        }
        let ready = dir.appendingPathComponent("helper-ready")
        try await waitForFile(ready, child: child, output: output)
        let expected = try JSONDecoder().decode(FedStateDocument.self, from: Data(contentsOf: ready))
        let contender = FedSQLiteStateStore(directoryURL: dir)
        do {
            _ = try await contender.open(localPublicKey: localKey)
            XCTFail("second process must get FedStateStoreInUse while helper owns the store")
            try await contender.close()
        } catch is FedStateStoreInUse {}
        try Data().write(to: dir.appendingPathComponent("helper-release"))
        let second = FedSQLiteStateStore(directoryURL: dir)
        let deadline = Date().addingTimeInterval(10)
        while true {
            do {
                let opened = try await second.open(localPublicKey: localKey)
                XCTAssertFalse(opened.created)
                XCTAssertEqual(opened.document, expected, "next owner must see the helper's durable writes")
                if !releaseByExit { XCTAssertTrue(child.isRunning, "helper must still be alive after explicit close") }
                break
            } catch is FedStateStoreInUse {
                guard Date() < deadline else { throw NSError(domain: "LockHolderTimeout", code: 1) }
                try await Task.sleep(nanoseconds: 10_000_000)
            }
        }
        let reserved = try await second.reserveEffectSequence()
        XCTAssertGreaterThan(reserved.value, expected.global.nextEffectSequence - 1)
        try await second.close()
        try Data().write(to: dir.appendingPathComponent("parent-reopened"))
        child.waitUntilExit()
        let diagnostics = String(decoding: try Data(contentsOf: output), as: UTF8.self)
        XCTAssertEqual(child.terminationStatus, 0, diagnostics)
    }

    private func waitForFile(_ file: URL, child: Process, output: URL) async throws {
        let deadline = Date().addingTimeInterval(10)
        while !FileManager.default.fileExists(atPath: file.path) {
            guard child.isRunning, Date() < deadline else {
                let diagnostics = String(decoding: try Data(contentsOf: output), as: UTF8.self)
                XCTFail("lock holder did not become ready: \(diagnostics)")
                throw NSError(domain: "LockHolderTimeout", code: 2)
            }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
    }
    #endif
}
