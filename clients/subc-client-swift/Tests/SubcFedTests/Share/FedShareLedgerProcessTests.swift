#if os(macOS)
import XCTest
import Darwin
@testable import SubcFed

final class FedShareLedgerProcessTests: XCTestCase {
    private let id = "019a0f00-1234-7000-8000-000000000001"

    private func directory() throws -> URL {
        let url = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent("target/scratch/share-process/\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }

    // Re-executed as a real second process. The file barrier deliberately keeps
    // a read-modify-write transaction open while the parent contends for flock.
    func testProcessHelper() throws {
        let env = ProcessInfo.processInfo.environment
        guard let path = env["SUBCFED_SHARE_PROCESS_DIRECTORY"] else { return }
        let dir = URL(fileURLWithPath: path)
        let ledger = shareTestLedger(directoryURL: dir)
        try ledger.withLedgerLock { transaction in
            let claim = try transaction.claim(id)
            try Data().write(to: dir.appendingPathComponent("ready"))
            let deadline = Date().addingTimeInterval(15)
            while !FileManager.default.fileExists(atPath: dir.appendingPathComponent("release").path) {
                guard Date() < deadline else { throw FedShareError.storage }
                usleep(5_000)
            }
            try transaction.record(FedShareOutcome(.refused, neverRan: true), shareID: id, claim: claim)
        }
    }

    private func launch(_ dir: URL) throws -> FedTestProcess {
        let child = Process()
        child.executableURL = URL(fileURLWithPath: "/usr/bin/xcrun")
        child.arguments = ["xctest", "-XCTest", "SubcFedTests.FedShareLedgerProcessTests/testProcessHelper",
                           Bundle(for: Self.self).bundleURL.path]
        var environment = ProcessInfo.processInfo.environment
        environment["SUBCFED_SHARE_PROCESS_DIRECTORY"] = dir.path
        child.environment = environment
        let log = dir.appendingPathComponent("child.log")
        FileManager.default.createFile(atPath: log.path, contents: nil)
        let handle = try FileHandle(forWritingTo: log)
        child.standardOutput = handle; child.standardError = handle
        let helper = try FedTestProcess(child)
        try handle.close()
        addTeardownBlock {
            if child.isRunning { kill(child.processIdentifier, SIGKILL) }
            _ = try await helper.waitForExit()
        }
        return helper
    }

    private func waitReady(_ dir: URL, child: Process) async throws {
        let deadline = Date().addingTimeInterval(10)
        while !FileManager.default.fileExists(atPath: dir.appendingPathComponent("ready").path) {
            guard child.isRunning, Date() < deadline else {
                XCTFail(String(decoding: try Data(contentsOf: dir.appendingPathComponent("child.log")), as: UTF8.self))
                throw FedShareError.storage
            }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
    }

    func testTwoProcessesContendingClaimsNewestWins() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let helper = try launch(dir), child = helper.process
        try await waitReady(dir, child: child)
        let started = expectation(description: "contender started")
        let finished = expectation(description: "contender finished")
        finished.isInverted = true
        let shareID = id
        let worker = Task {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<UInt64, Error>) in
                // Run blocking flock on a separate worker so the async test can
                // write the release file and let the child unlock its transaction.
                DispatchQueue(label: "share-ledger-contender").async {
                    started.fulfill()
                    defer { finished.fulfill() }
                    continuation.resume(with: Result {
                        let claim = try ledger.claim(shareID)
                        try ledger.record(FedShareOutcome(.sent), shareID: shareID, claim: claim)
                        return claim
                    })
                }
            }
        }
        await fulfillment(of: [started], timeout: 2)
        await fulfillment(of: [finished], timeout: 0.2)
        try Data().write(to: dir.appendingPathComponent("release"))
        let status = try await helper.waitForExit()
        XCTAssertEqual(status, 0)
        let claim = try await worker.value
        XCTAssertEqual(claim, 2)
        XCTAssertEqual(try ledger.entry(shareID: id)?.claim, 2)
        XCTAssertEqual(try ledger.entry(shareID: id)?.lastOutcome?.disposition, .sent)
    }

    func testKilledLockHolderReleasesImmediately() async throws {
        let dir = try directory(), ledger = shareTestLedger(directoryURL: dir)
        let helper = try launch(dir), child = helper.process
        try await waitReady(dir, child: child)
        XCTAssertEqual(kill(child.processIdentifier, SIGKILL), 0)
        _ = try await helper.waitForExit()
        let start = DispatchTime.now().uptimeNanoseconds
        let claim = try ledger.claim(id)
        XCTAssertEqual(claim, 1, "the killed transaction never committed")
        XCTAssertLessThan(DispatchTime.now().uptimeNanoseconds - start, 1_000_000_000)
    }
}
#endif
