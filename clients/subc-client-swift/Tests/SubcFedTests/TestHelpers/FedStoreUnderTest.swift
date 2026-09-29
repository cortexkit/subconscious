import Foundation
import XCTest
@testable import SubcFed

/// Which store a parameterized suite runs against.
///
/// The store, reconciliation and kill-window suites were written against the
/// memory store (where a test only needs somewhere to keep state) and the JSON
/// file store (where it reopens from disk). Each such suite declares
/// `class var storeUnderTest`, defaulting to `.asWritten`, and gets every store
/// through this type; a `...SQLiteTests` subclass overrides it with `.sqlite`
/// and so reruns every test of the suite against the SQLite store.
enum FedStoreUnderTest: Equatable {
    /// The memory store where a test wants a scratch store, the JSON file
    /// store where it wants a durable one: the suite exactly as written.
    case asWritten
    /// The SQLite store everywhere.
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
        switch self {
        case .asWritten:
            return FedAtomicFileStateStore(directoryURL: directory)
        case .sqlite:
            return FedSQLiteStateStore(directoryURL: directory)
        }
    }

    static func temporaryDirectory(removedAfter testCase: XCTestCase) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("subcfed-store-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        testCase.addTeardownBlock { try? FileManager.default.removeItem(at: url) }
        return url
    }
}
