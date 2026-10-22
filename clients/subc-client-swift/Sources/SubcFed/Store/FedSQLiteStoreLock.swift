import Foundation
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

/// Owns one open file description, so even two openers in one process contend.
/// Never unlink the lock file: replacing its inode would let two owners lock
/// different files for the same database.
final class FedSQLiteStoreLock {
    private let descriptor: Int32

    init(url: URL) throws {
        let fd = open(url.path, O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard fd >= 0 else { throw FedFailure.storeUnavailable }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            let code = errno
            _ = close(fd)
            if code == EWOULDBLOCK || code == EAGAIN { throw FedStateStoreInUse() }
            throw FedFailure.storeUnavailable
        }
        descriptor = fd
    }

    deinit {
        // Closing the sole descriptor releases flock as well as the file.
        _ = close(descriptor)
    }
}
