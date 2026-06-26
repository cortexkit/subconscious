import Foundation
import Darwin

// Minimal blocking POSIX-socket transport for the spike. The wire logic
// (connection file, handshake, envelope, control RPC) is written against the
// `Transport` protocol, so the production client can swap in an async
// Network.framework (NWConnection) implementation without touching any
// byte-level code.

public protocol Transport {
    func writeAll(_ data: Data) throws
    func readExact(_ n: Int) throws -> Data
    func close()
}

public struct TransportError: Error { public let message: String }

public final class POSIXTransport: Transport {
    private let fd: Int32

    public init(host: String, port: UInt16) throws {
        fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw TransportError(message: "socket() failed errno \(errno)") }

        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        guard inet_pton(AF_INET, host, &addr.sin_addr) == 1 else {
            Darwin.close(fd)
            throw TransportError(message: "inet_pton failed for host \(host)")
        }

        let rc = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard rc == 0 else {
            Darwin.close(fd)
            throw TransportError(message: "connect() failed errno \(errno)")
        }
    }

    public func writeAll(_ data: Data) throws {
        try data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.baseAddress else { return }
            var off = 0
            while off < data.count {
                let n = Darwin.send(fd, base + off, data.count - off, 0)
                if n <= 0 { throw TransportError(message: "send() failed errno \(errno)") }
                off += n
            }
        }
    }

    public func readExact(_ n: Int) throws -> Data {
        guard n > 0 else { return Data() }
        var buf = [UInt8](repeating: 0, count: n)
        var got = 0
        while got < n {
            let r = buf.withUnsafeMutableBytes { ptr -> Int in
                Darwin.recv(fd, ptr.baseAddress!.advanced(by: got), n - got, 0)
            }
            if r == 0 { throw TransportError(message: "peer closed (EOF) after \(got)/\(n) bytes") }
            if r < 0 { throw TransportError(message: "recv() failed errno \(errno)") }
            got += r
        }
        return Data(buf)
    }

    public func close() { Darwin.close(fd) }
}
