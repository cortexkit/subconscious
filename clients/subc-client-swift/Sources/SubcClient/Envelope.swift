import Foundation

// Byte-for-byte port of subc-protocol's 17-byte envelope header.
// Source of truth: crates/subc-protocol/src/lib.rs (and the TS mirror in
// clients/subc-client/src/envelope.ts). Field offsets, the little-endian
// encoding, and the frame-type/flag numbering must stay in lock-step with the
// Rust; a one-byte drift desynchronizes every frame on the wire.

public let PROTOCOL_VERSION: UInt8 = 1
public let HEADER_LEN = 17
public let FROZEN_PREFIX_LEN = 5
public let MAX_FRAME_BODY_LEN = 64 * 1024 * 1024

/// `type` byte at offset 5.
public enum FrameType: UInt8 {
    case request = 0
    case response = 1
    case push = 2
    case streamData = 3
    case streamEnd = 4
    case error = 5
    case cancel = 6
    case ping = 7
    case pong = 8
    case hello = 9
    case helloAck = 10
    case goodbye = 11
}

/// Scheduling priority carried in flags bits 1-2.
public enum Priority: UInt8 {
    case passive = 0
    case interactive = 1
    case background = 2
}

/// Build the flags byte from typed components (mirrors Flags::new).
/// bit0 = binary, bits1-2 = priority, bit3 = last.
public func buildFlags(binary: Bool, priority: Priority, last: Bool) -> UInt8 {
    var b: UInt8 = 0
    if binary { b |= 0b0000_0001 }
    b |= priority.rawValue << 1
    if last { b |= 0b0000_1000 }
    return b
}

public struct EnvelopeHeader {
    public var len: UInt32
    public var ver: UInt8
    public var ty: FrameType
    public var flags: UInt8
    public var channel: UInt16
    public var corr: UInt64

    public init(len: UInt32, ver: UInt8, ty: FrameType, flags: UInt8, channel: UInt16, corr: UInt64) {
        self.len = len
        self.ver = ver
        self.ty = ty
        self.flags = flags
        self.channel = channel
        self.corr = corr
    }
}

public struct Frame {
    public var header: EnvelopeHeader
    public var body: Data

    public init(header: EnvelopeHeader, body: Data) {
        self.header = header
        self.body = body
    }
}

public struct DecodeError: Error { public let message: String }

/// Serialize a header to its fixed 17-byte little-endian form.
public func encodeHeader(_ h: EnvelopeHeader) -> Data {
    var d = Data()
    withUnsafeBytes(of: h.len.littleEndian) { d.append(contentsOf: $0) }
    d.append(h.ver)
    d.append(h.ty.rawValue)
    d.append(h.flags)
    withUnsafeBytes(of: h.channel.littleEndian) { d.append(contentsOf: $0) }
    withUnsafeBytes(of: h.corr.littleEndian) { d.append(contentsOf: $0) }
    return d
}

/// Decode a header from a 17-byte buffer. Minimal validation for the spike
/// (version + frame-type recognized); the full reserved-bit / pure-header
/// validation lands with the golden-vector test suite.
public func decodeHeader(_ bytes: Data) throws -> EnvelopeHeader {
    let b = [UInt8](bytes)
    guard b.count >= HEADER_LEN else { throw DecodeError(message: "header too short: \(b.count) bytes") }
    guard b[4] == PROTOCOL_VERSION else { throw DecodeError(message: "unsupported envelope version \(b[4])") }
    let len = UInt32(b[0]) | (UInt32(b[1]) << 8) | (UInt32(b[2]) << 16) | (UInt32(b[3]) << 24)
    guard let ty = FrameType(rawValue: b[5]) else { throw DecodeError(message: "unknown frame type byte \(b[5])") }
    let channel = UInt16(b[7]) | (UInt16(b[8]) << 8)
    var corr: UInt64 = 0
    for i in 0..<8 { corr |= UInt64(b[9 + i]) << (8 * UInt64(i)) }
    return EnvelopeHeader(len: len, ver: b[4], ty: ty, flags: b[6], channel: channel, corr: corr)
}

/// Encode a frame to wire bytes: 17-byte header followed by `len` body bytes.
public func encodeFrame(ty: FrameType, flags: UInt8, channel: UInt16, corr: UInt64, body: Data) -> Data {
    let header = EnvelopeHeader(len: UInt32(body.count), ver: PROTOCOL_VERSION, ty: ty, flags: flags, channel: channel, corr: corr)
    var out = encodeHeader(header)
    out.append(body)
    return out
}
