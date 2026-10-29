#if os(macOS)
import XCTest
import Foundation

final class FedTestProcessTests: XCTestCase {
    func testExitBeforeWaitIsRetainedForEveryWaiter() async throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/true")
        let child = try FedTestProcess(process)
        let deadline = Date().addingTimeInterval(2)
        while !child.hasExited {
            guard Date() < deadline else { return XCTFail("fast child did not exit") }
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        let completed = expectation(description: "late waiters see the saved exit")
        completed.expectedFulfillmentCount = 2
        for _ in 0..<2 {
            Task {
                do {
                    let status = try await child.waitForExit(timeout: 2)
                    XCTAssertEqual(status, 0)
                } catch { XCTFail("late exit wait failed: \(error)") }
                completed.fulfill()
            }
        }
        await fulfillment(of: [completed], timeout: 2)
    }

    func testConcurrentWaitersAreAllResumed() async throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/cat")
        let input = Pipe()
        process.standardInput = input
        process.standardOutput = FileHandle.nullDevice
        let child = try FedTestProcess(process)
        addTeardownBlock {
            if process.isRunning { process.terminate() }
            _ = try await child.waitForExit()
        }
        let started = expectation(description: "waiters started")
        let completed = expectation(description: "all waiters resumed")
        started.expectedFulfillmentCount = 2
        completed.expectedFulfillmentCount = 2
        for _ in 0..<2 {
            Task {
                started.fulfill()
                do {
                    let status = try await child.waitForExit(timeout: 2)
                    XCTAssertEqual(status, 0)
                } catch { XCTFail("concurrent exit wait failed: \(error)") }
                completed.fulfill()
            }
        }
        await fulfillment(of: [started], timeout: 2)
        try input.fileHandleForWriting.close()
        await fulfillment(of: [completed], timeout: 2)
    }

    func testLaunchFailureThrowsWithoutWaitingForExit() {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/no-such-test-executable/\(UUID().uuidString)")
        XCTAssertThrowsError(try FedTestProcess(process))
        XCTAssertNil(process.terminationHandler)
    }

    func testTimedOutWaitDoesNotConsumeLaterExit() async throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/cat")
        let input = Pipe()
        process.standardInput = input
        process.standardOutput = FileHandle.nullDevice
        let child = try FedTestProcess(process)
        addTeardownBlock {
            if process.isRunning { process.terminate() }
            _ = try await child.waitForExit()
        }
        do {
            _ = try await child.waitForExit(timeout: 0.01)
            XCTFail("child should remain alive while its input is open")
        } catch FedTestProcess.WaitError.timedOut {}
        try input.fileHandleForWriting.close()
        let status = try await child.waitForExit(timeout: 2)
        XCTAssertEqual(status, 0)
    }
}
#endif
