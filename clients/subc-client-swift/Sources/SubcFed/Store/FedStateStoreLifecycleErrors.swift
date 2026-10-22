import Foundation

/// Another store instance or process owns this directory. Keep the existing
/// store intact and retry opening after its owner has closed it.
public struct FedStateStoreInUse: Error, LocalizedError, Sendable, Equatable {
    public init() {}

    public var errorDescription: String? {
        "The fed state store is in use by another opener. Retry after it closes the store."
    }
}

/// The SQLite store is not open. A closed actor cannot be reopened: create a
/// new store and new clients for the next ownership period over the same files.
public struct FedStateStoreClosed: Error, LocalizedError, Sendable, Equatable {
    public init() {}

    public var errorDescription: String? {
        "The fed state store is not open or has been closed. After close, create a new store and new clients."
    }
}
