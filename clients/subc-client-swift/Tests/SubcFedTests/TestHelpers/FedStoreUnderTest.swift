import Foundation
import XCTest
@testable import SubcFed

/// Selects memory scratch stores with SQLite for persistence tests, or SQLite
/// for every test. SQLite subclasses rerun scratch-store behavior on disk.
enum FedStoreUnderTest: Equatable {
    case asWritten
    case sqlite

    /// A store for a test that never reopens it. For `.sqlite` the database
    /// lives in a temporary directory removed when `testCase` finishes.
    func scratchStore(for testCase: XCTestCase) throws -> any FedStateStore {
        switch self {
        case .asWritten:
            return FedMemoryStateStore()
        case .sqlite:
            return durableStore(in: try Self.temporaryDirectory(removedAfter: testCase))
        }
    }

    /// A store over `directory` that a test may reopen with a second call.
    func durableStore(in directory: URL) -> any FedStateStore {
        FedSQLiteStateStore(directoryURL: directory)
    }

    static func temporaryDirectory(removedAfter testCase: XCTestCase) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-store-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        testCase.addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
