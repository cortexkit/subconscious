import Foundation
import SQLite3
@testable import SubcFed

/// Seeds SQLite fixtures directly so storage tests do not depend on a legacy importer.
enum FedSQLiteTestSeed {
    static func write(_ document: FedStateDocument, in directory: URL) throws {
        let db = try FedSQLiteConnection.open(
            path: directory.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path,
            flags: FedSQLiteStateStore.openFlags | SQLITE_OPEN_CREATE
        )
        try db.execute("PRAGMA auto_vacuum=INCREMENTAL; BEGIN IMMEDIATE;")
        try db.execute(FedSQLiteStoreRows.schema)
        try FedSQLiteStoreRows.insert(document, into: db)
        try db.execute("COMMIT")
        try db.close()
    }
}
