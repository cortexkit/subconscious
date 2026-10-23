import Foundation
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

/// Failures of the independent App Group share files and wire protocol.
public enum FedShareError: Error, Equatable, Sendable {
    case storage
    case invalidRequest
    case invalidFrame
    case markConflict(shareID: String)
    case moved(shareID: String)
    case expiredTransaction
    case claimOverflow
}

// The lock inode is permanent. Each transaction opens its own description so
// threads and processes contend alike; the kernel releases it on process death.
enum FedShareFiles {
    static func locked<T>(at url: URL, _ body: () throws -> T) throws -> T {
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let fd = open(url.path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard fd >= 0 else { throw FedShareError.storage }
        defer { _ = close(fd) }
        var result: Int32
        repeat { result = flock(fd, LOCK_EX) } while result != 0 && errno == EINTR
        guard result == 0 else { throw FedShareError.storage }
        defer { _ = flock(fd, LOCK_UN) }
        return try body()
    }

    static func encode<T: Encodable>(_ value: T) throws -> Data {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        return try encoder.encode(value)
    }

    // Rename publishes one complete snapshot; syncing both the file and its
    // parent makes a committed mark survive a crash before the network write.
    static func replace(_ data: Data, at url: URL) throws {
        let parent = url.deletingLastPathComponent()
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        let temporary = parent.appendingPathComponent(".share-\(UUID().uuidString)")
        let fd = open(temporary.path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard fd >= 0 else { throw FedShareError.storage }
        defer { _ = close(fd); _ = unlink(temporary.path) }
        try data.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let count = write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
                if count < 0 && errno == EINTR { continue }
                guard count > 0 else { throw FedShareError.storage }
                offset += count
            }
        }
        guard fsync(fd) == 0, rename(temporary.path, url.path) == 0 else { throw FedShareError.storage }
        let directory = open(parent.path, O_RDONLY | O_CLOEXEC)
        guard directory >= 0 else { throw FedShareError.storage }
        defer { _ = close(directory) }
        guard fsync(directory) == 0 else { throw FedShareError.storage }
    }
}
