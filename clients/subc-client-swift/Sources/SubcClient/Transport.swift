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

// The production connection probe can bound I/O without adding requirements to
// the public Transport protocol or changing ordinary blocking reads.
protocol CapabilityProbeTransport: AnyObject, Transport {
    var capabilityProbeDeadline: TimeInterval? { get set }
}

public final class POSIXTransport: Transport, CapabilityProbeTransport {
    private var fd: Int32
    var capabilityProbeDeadline: TimeInterval?
    private var bufferedRead = Data()
    private var frameCheckpoint: Data?

    init(connectedSocket: Int32) { fd = connectedSocket }

    public init(host: String, port: UInt16) throws {
        fd = socket(AF_INET, SOCK_STREAM, 0)
        guard fd >= 0 else { throw TransportError(message: "socket() failed errno \(errno)") }

        var noSigPipe: Int32 = 1
        let noSigPipeResult = setsockopt(
            fd,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout<Int32>.size)
        )
        guard noSigPipeResult == 0 else {
            Darwin.close(fd)
            throw TransportError(message: "setsockopt(SO_NOSIGPIPE) failed errno \(errno)")
        }

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
                try waitForProbeIO(events: Int16(POLLOUT))
                let n = Darwin.send(fd, base + off, data.count - off, capabilityProbeDeadline == nil ? 0 : MSG_DONTWAIT)
                if n < 0, capabilityProbeDeadline != nil, errno == EAGAIN || errno == EINTR { continue }
                if n <= 0 { throw TransportError(message: "send() failed errno \(errno)") }
                off += n
            }
        }
    }

    public func readExact(_ n: Int) throws -> Data {
        guard n > 0 else { return Data() }
        var data = Data(count: n)
        var got = 0
        if !bufferedRead.isEmpty {
            got = min(n, bufferedRead.count)
            let prefix = bufferedRead.prefix(got)
            data.replaceSubrange(0..<got, with: prefix)
            frameCheckpoint?.append(prefix)
            bufferedRead.removeFirst(got)
        }
        while got < n {
            try waitForProbeIO(events: Int16(POLLIN))
            let r = data.withUnsafeMutableBytes { (ptr: UnsafeMutableRawBufferPointer) -> Int in
                Darwin.recv(fd, ptr.baseAddress!.advanced(by: got), n - got, capabilityProbeDeadline == nil ? 0 : MSG_DONTWAIT)
            }
            if r == 0 { throw TransportError(message: "peer closed (EOF) after \(got)/\(n) bytes") }
            if r < 0, capabilityProbeDeadline != nil, errno == EAGAIN || errno == EINTR { continue }
            if r < 0 { throw TransportError(message: "recv() failed errno \(errno)") }
            frameCheckpoint?.append(data[got..<(got + r)])
            got += r
        }
        return data
    }

    private func waitForProbeIO(events: Int16) throws {
        guard let deadline = capabilityProbeDeadline else { return }
        while true {
            let remaining = deadline - ProcessInfo.processInfo.systemUptime
            guard remaining > 0 else { throw TransportError(message: "capability probe timed out") }
            var descriptor = pollfd(fd: fd, events: events, revents: 0)
            let milliseconds = Int32(min(Double(Int32.max), ceil(remaining * 1_000)))
            let result = Darwin.poll(&descriptor, 1, milliseconds)
            if result > 0 { return }
            if result < 0 && errno != EINTR { throw TransportError(message: "poll() failed errno \(errno)") }
        }
    }

    // A probe may time out in the middle of a frame. Preserve its consumed bytes
    // so a late describe reply can be read and skipped by the next request.
    func beginProbeFrame() { frameCheckpoint = capabilityProbeDeadline == nil ? nil : Data() }
    func commitProbeFrame() { frameCheckpoint = nil }
    func rewindProbeFrame() {
        if let frameCheckpoint { bufferedRead = frameCheckpoint + bufferedRead }
        frameCheckpoint = nil
    }

    public func close() {
        guard fd >= 0 else { return }
        Darwin.close(fd)
        fd = -1
    }

    deinit {
        close()
    }
}

public enum FrameReadError: Error, Equatable {
    case bodyTooLarge(len: UInt32, max: Int)
}

/// Read one frame without committing to a version-specific header length until
/// the frozen prefix has been validated. This makes a stale 17-byte pure-header
/// frame fail on its version byte instead of waiting for four bytes that will
/// never arrive.
func readFrame(from transport: Transport) throws -> Frame {
    let checkpoint = transport as? POSIXTransport
    checkpoint?.beginProbeFrame()
    do {
        let prefix = try transport.readExact(FROZEN_PREFIX_LEN)
        let bodyLength = try decodeFrozenPrefix(prefix)
        guard UInt64(bodyLength) <= UInt64(MAX_FRAME_BODY_LEN) else {
            throw FrameReadError.bodyTooLarge(len: bodyLength, max: MAX_FRAME_BODY_LEN)
        }

        var headerBytes = prefix
        headerBytes.append(try transport.readExact(HEADER_LEN - FROZEN_PREFIX_LEN))
        let header = try decodeHeader(headerBytes)
        let body = bodyLength > 0 ? try transport.readExact(Int(bodyLength)) : Data()
        checkpoint?.commitProbeFrame()
        return Frame(header: header, body: body)
    } catch {
        checkpoint?.rewindProbeFrame()
        throw error
    }
}
