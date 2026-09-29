import Foundation
import SQLite3

/// A failed SQLite call, with the extended result code and SQLite's message.
struct FedSQLiteError: Error, CustomStringConvertible {
    let code: Int32
    let message: String

    /// The primary result code (the low byte of an extended code).
    var primaryCode: Int32 { code & 0xFF }

    var description: String { "SQLite error \(code): \(message)" }

    /// Whether this failure means the file could not be reached rather than
    /// that its contents are bad.
    ///
    /// Before the first unlock after a restart, iOS refuses to open or read a
    /// file of the default protection class, and SQLite reports that as a
    /// can't-open or I/O error. Permission, busy and read-only failures are
    /// grouped with them: in every one of these cases the database may be
    /// perfectly intact, so the caller must retry later rather than conclude
    /// there is no database.
    var isUnreachable: Bool {
        switch primaryCode {
        case SQLITE_CANTOPEN, SQLITE_IOERR, SQLITE_PERM, SQLITE_AUTH,
             SQLITE_BUSY, SQLITE_LOCKED, SQLITE_READONLY:
            return true
        default:
            return false
        }
    }

    /// Whether SQLite judged the file itself to be damaged or not a database.
    var isCorruption: Bool {
        primaryCode == SQLITE_CORRUPT || primaryCode == SQLITE_NOTADB
    }

    var isConstraintViolation: Bool { primaryCode == SQLITE_CONSTRAINT }
}

/// SQLite copies text and blob arguments when given this destructor, so the
/// Swift buffers they came from need not outlive the bind call. The C macro
/// `SQLITE_TRANSIENT` is not imported into Swift, so it is rebuilt here.
private let sqliteTransient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

/// One SQLite connection. Not thread-safe; the owning actor serialises use.
final class FedSQLiteConnection {
    private(set) var handle: OpaquePointer?

    private init(handle: OpaquePointer) {
        self.handle = handle
    }

    /// Opens `path` with `flags`. On failure the half-opened handle SQLite
    /// returns is closed before the error is thrown.
    static func open(path: String, flags: Int32) throws -> FedSQLiteConnection {
        var raw: OpaquePointer?
        let rc = sqlite3_open_v2(path, &raw, flags, nil)
        guard rc == SQLITE_OK, let raw else {
            let error = FedSQLiteError(
                code: raw.map { sqlite3_extended_errcode($0) } ?? rc,
                message: raw.map { String(cString: sqlite3_errmsg($0)) } ?? "open failed"
            )
            if let raw { sqlite3_close_v2(raw) }
            throw error
        }
        sqlite3_extended_result_codes(raw, 1)
        return FedSQLiteConnection(handle: raw)
    }

    deinit {
        if let handle { sqlite3_close_v2(handle) }
    }

    /// Closes the connection now, reporting a failure instead of deferring it.
    func close() throws {
        guard let handle else { return }
        let rc = sqlite3_close(handle)
        guard rc == SQLITE_OK else { throw lastError(rc) }
        self.handle = nil
    }

    /// True while a transaction is open on this connection.
    var isInsideTransaction: Bool {
        guard let handle else { return false }
        return sqlite3_get_autocommit(handle) == 0
    }

    /// Rows changed by the most recent INSERT, UPDATE or DELETE.
    var changes: Int { handle.map { Int(sqlite3_changes($0)) } ?? 0 }

    func setBusyTimeout(milliseconds: Int32) {
        if let handle { sqlite3_busy_timeout(handle, milliseconds) }
    }

    /// Keeps the `-wal` and `-shm` files in place when the last connection
    /// closes, instead of deleting them and creating new ones on the next
    /// open. A file that is deleted and recreated loses the attributes set on
    /// it, such as the exclusion from backup.
    func keepWriteAheadLogFiles() throws {
        guard let handle else { throw FedSQLiteError(code: SQLITE_MISUSE, message: "closed") }
        var enabled: Int32 = 1
        let rc = sqlite3_file_control(handle, "main", SQLITE_FCNTL_PERSIST_WAL, &enabled)
        guard rc == SQLITE_OK else { throw lastError(rc) }
    }

    /// Runs one or more statements that bind no parameters.
    func execute(_ sql: String) throws {
        guard let handle else { throw FedSQLiteError(code: SQLITE_MISUSE, message: "closed") }
        var message: UnsafeMutablePointer<CChar>?
        let rc = sqlite3_exec(handle, sql, nil, nil, &message)
        if rc != SQLITE_OK {
            let text = message.map { String(cString: $0) } ?? String(cString: sqlite3_errmsg(handle))
            sqlite3_free(message)
            throw FedSQLiteError(code: sqlite3_extended_errcode(handle), message: text)
        }
    }

    func prepare(_ sql: String) throws -> FedSQLiteStatement {
        guard let handle else { throw FedSQLiteError(code: SQLITE_MISUSE, message: "closed") }
        var statement: OpaquePointer?
        let rc = sqlite3_prepare_v2(handle, sql, -1, &statement, nil)
        guard rc == SQLITE_OK, let statement else { throw lastError(rc) }
        return FedSQLiteStatement(statement: statement, connection: self)
    }

    /// Runs one statement to completion with `bindings`, discarding any rows.
    func run(_ sql: String, _ bindings: [FedSQLiteValue] = []) throws {
        let statement = try prepare(sql)
        try statement.bind(bindings)
        while try statement.step() {}
    }

    /// Reads the single text value a PRAGMA query returns.
    func pragmaText(_ name: String) throws -> String? {
        let statement = try prepare("PRAGMA \(name)")
        guard try statement.step() else { return nil }
        return statement.text(0)
    }

    func lastError(_ rc: Int32) -> FedSQLiteError {
        guard let handle else { return FedSQLiteError(code: rc, message: "closed") }
        return FedSQLiteError(
            code: sqlite3_extended_errcode(handle),
            message: String(cString: sqlite3_errmsg(handle))
        )
    }
}

/// A value bound to a statement parameter.
enum FedSQLiteValue {
    case null
    case integer(Int64)
    case text(String)
    case blob(Data)

    static func textOrNull(_ value: String?) -> FedSQLiteValue { value.map { .text($0) } ?? .null }
    static func blobOrNull(_ value: Data?) -> FedSQLiteValue { value.map { .blob($0) } ?? .null }
    /// Sequence numbers are unsigned in Swift and signed in SQLite; the bit
    /// pattern is stored so every value round-trips exactly.
    static func unsigned(_ value: UInt64) -> FedSQLiteValue { .integer(Int64(bitPattern: value)) }
    static func unsignedOrNull(_ value: UInt64?) -> FedSQLiteValue { value.map { .unsigned($0) } ?? .null }
}

/// One prepared statement, finalized when released.
final class FedSQLiteStatement {
    private let statement: OpaquePointer
    private let connection: FedSQLiteConnection

    fileprivate init(statement: OpaquePointer, connection: FedSQLiteConnection) {
        self.statement = statement
        self.connection = connection
    }

    deinit {
        sqlite3_finalize(statement)
    }

    func bind(_ values: [FedSQLiteValue]) throws {
        for (offset, value) in values.enumerated() {
            let index = Int32(offset + 1)
            let rc: Int32
            switch value {
            case .null:
                rc = sqlite3_bind_null(statement, index)
            case .integer(let integer):
                rc = sqlite3_bind_int64(statement, index, integer)
            case .text(let text):
                rc = sqlite3_bind_text(statement, index, text, -1, sqliteTransient)
            case .blob(let data):
                if data.isEmpty {
                    // An empty Data may have no base address, and binding a nil
                    // pointer stores NULL; a zero-length blob keeps "empty"
                    // distinct from "absent".
                    rc = sqlite3_bind_zeroblob(statement, index, 0)
                } else {
                    rc = data.withUnsafeBytes { bytes in
                        sqlite3_bind_blob(statement, index, bytes.baseAddress, Int32(bytes.count), sqliteTransient)
                    }
                }
            }
            guard rc == SQLITE_OK else { throw connection.lastError(rc) }
        }
    }

    /// Advances to the next row. Returns false once the statement is done.
    func step() throws -> Bool {
        let rc = sqlite3_step(statement)
        switch rc {
        case SQLITE_ROW: return true
        case SQLITE_DONE: return false
        default: throw connection.lastError(rc)
        }
    }

    /// The column as whichever type SQLite holds it in.
    func value(_ column: Int32) -> FedSQLiteValue {
        switch sqlite3_column_type(statement, column) {
        case SQLITE_INTEGER: return .integer(sqlite3_column_int64(statement, column))
        case SQLITE_TEXT: return .textOrNull(text(column))
        case SQLITE_BLOB: return .blobOrNull(blob(column))
        default: return .null
        }
    }

    func isNull(_ column: Int32) -> Bool {
        sqlite3_column_type(statement, column) == SQLITE_NULL
    }

    func int64(_ column: Int32) -> Int64? {
        isNull(column) ? nil : sqlite3_column_int64(statement, column)
    }

    func unsigned(_ column: Int32) -> UInt64? {
        int64(column).map { UInt64(bitPattern: $0) }
    }

    func text(_ column: Int32) -> String? {
        guard !isNull(column), let bytes = sqlite3_column_text(statement, column) else { return nil }
        return String(cString: bytes)
    }

    func blob(_ column: Int32) -> Data? {
        if isNull(column) { return nil }
        let count = Int(sqlite3_column_bytes(statement, column))
        // A zero-length blob has no data pointer; it is still an empty value.
        guard count > 0, let bytes = sqlite3_column_blob(statement, column) else { return Data() }
        return Data(bytes: bytes, count: count)
    }
}
