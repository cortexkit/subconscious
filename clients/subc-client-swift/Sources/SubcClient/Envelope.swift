import Foundation

// Byte-for-byte mirror of subc-protocol's frozen v2 envelope. The first five
// bytes stay stable so a reader can reject an unsupported version before it
// waits for that version's full header.

public let PROTOCOL_VERSION: UInt8 = 2
public let HEADER_LEN = 21
public let FROZEN_PREFIX_LEN = 5
public let MAX_FRAME_BODY_LEN = 64 * 1024 * 1024
public let DAEMON_ORIGIN_FLAG: UInt8 = 0x40

/// `type` byte at offset 5.
public enum FrameType: UInt8, Equatable, Sendable {
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

    public var isPureHeader: Bool {
        switch self {
        case .cancel, .ping, .pong, .goodbye: true
        default: false
        }
    }
}

/// Scheduling priority carried in flags bits 1-2.
public enum Priority: UInt8, Equatable, Sendable {
    case passive = 0
    case interactive = 1
    case background = 2
}

/// Admission behavior carried in flags bits 4-5.
public enum AdmissionClass: UInt8, Equatable, Sendable {
    case normal = 0
    case expedite = 1
    case sheddable = 2
}

/// Build the flags byte from typed components.
public func buildFlags(
    binary: Bool,
    priority: Priority,
    last: Bool,
    admissionClass: AdmissionClass = .normal
) -> UInt8 {
    var bits: UInt8 = 0
    if binary { bits |= 0b0000_0001 }
    bits |= priority.rawValue << 1
    if last { bits |= 0b0000_1000 }
    bits |= admissionClass.rawValue << 4
    return bits
}

public struct EnvelopeHeader: Equatable, Sendable {
    public var len: UInt32
    public var ver: UInt8
    public var ty: FrameType
    public var flags: UInt8
    public var channel: UInt16
    public var epoch: UInt32
    public var corr: UInt64

    /// Typed view of flags bits 4-5. Invalid raw headers return nil until decode rejects them.
    public var admissionClass: AdmissionClass? {
        AdmissionClass(rawValue: (flags >> 4) & 0b11)
    }

    public var daemonOrigin: Bool {
        flags & DAEMON_ORIGIN_FLAG != 0
    }

    public init(
        len: UInt32,
        ver: UInt8,
        ty: FrameType,
        flags: UInt8,
        channel: UInt16,
        epoch: UInt32,
        corr: UInt64
    ) {
        self.len = len
        self.ver = ver
        self.ty = ty
        self.flags = flags
        self.channel = channel
        self.epoch = epoch
        self.corr = corr
    }
}

public struct Frame: Equatable, Sendable {
    public var header: EnvelopeHeader
    public var body: Data

    public init(header: EnvelopeHeader, body: Data) {
        self.header = header
        self.body = body
    }
}

public enum FrameEncodeError: Error, Equatable {
    case bodyTooLarge(len: Int, max: Int)
}

public enum DecodeError: Error, Equatable, CustomStringConvertible {
    case tooShortForPrefix(have: Int)
    case unsupportedVersion(ver: UInt8)
    case tooShortForHeader(have: Int, need: Int)
    case unknownFrameType(byte: UInt8)
    case reservedFlagBits(flags: UInt8)
    case reservedPriorityBits(flags: UInt8)
    case reservedAdmissionClass(flags: UInt8)
    case sheddableIllegalFrameType(ty: FrameType, flags: UInt8)
    case nonzeroEpochOnControlChannel(epoch: UInt32)
    case pureHeaderFrameWithBody(ty: FrameType, len: UInt32)

    public var description: String {
        switch self {
        case let .tooShortForPrefix(have):
            "too short for frozen prefix: have \(have), need \(FROZEN_PREFIX_LEN)"
        case let .unsupportedVersion(ver):
            "unsupported envelope version \(ver)"
        case let .tooShortForHeader(have, need):
            "too short for header: have \(have), need \(need)"
        case let .unknownFrameType(byte):
            "unknown frame type byte \(byte)"
        case let .reservedFlagBits(flags):
            "reserved flag bits set in \(flags)"
        case let .reservedPriorityBits(flags):
            "reserved priority bits set in \(flags)"
        case let .reservedAdmissionClass(flags):
            "reserved admission class in \(flags)"
        case let .sheddableIllegalFrameType(ty, flags):
            "sheddable admission class is illegal for frame type \(ty) in \(flags)"
        case let .nonzeroEpochOnControlChannel(epoch):
            "control channel carried nonzero epoch \(epoch)"
        case let .pureHeaderFrameWithBody(ty, len):
            "pure-header frame \(ty) declared \(len) body bytes"
        }
    }
}

/// Decode and validate the frozen prefix without requiring the rest of a frame.
/// The body length is returned only after the exact supported version is known.
func decodeFrozenPrefix(_ bytes: Data) throws -> UInt32 {
    let raw = [UInt8](bytes)
    guard raw.count >= FROZEN_PREFIX_LEN else {
        throw DecodeError.tooShortForPrefix(have: raw.count)
    }
    guard raw[4] == PROTOCOL_VERSION else {
        throw DecodeError.unsupportedVersion(ver: raw[4])
    }
    return UInt32(raw[0])
        | (UInt32(raw[1]) << 8)
        | (UInt32(raw[2]) << 16)
        | (UInt32(raw[3]) << 24)
}

/// Serialize a header to its fixed 21-byte little-endian form.
public func encodeHeader(_ header: EnvelopeHeader) -> Data {
    var data = Data()
    withUnsafeBytes(of: header.len.littleEndian) { data.append(contentsOf: $0) }
    data.append(header.ver)
    data.append(header.ty.rawValue)
    data.append(header.flags)
    withUnsafeBytes(of: header.channel.littleEndian) { data.append(contentsOf: $0) }
    withUnsafeBytes(of: header.epoch.littleEndian) { data.append(contentsOf: $0) }
    withUnsafeBytes(of: header.corr.littleEndian) { data.append(contentsOf: $0) }
    return data
}

/// Decode a v2 header and apply the same validation ordering and taxonomy as
/// subc-protocol. Version validation needs only the frozen five-byte prefix.
public func decodeHeader(_ bytes: Data) throws -> EnvelopeHeader {
    let raw = [UInt8](bytes)
    _ = try decodeFrozenPrefix(bytes)
    guard raw.count >= HEADER_LEN else {
        throw DecodeError.tooShortForHeader(have: raw.count, need: HEADER_LEN)
    }

    let len = UInt32(raw[0])
        | (UInt32(raw[1]) << 8)
        | (UInt32(raw[2]) << 16)
        | (UInt32(raw[3]) << 24)
    guard let ty = FrameType(rawValue: raw[5]) else {
        throw DecodeError.unknownFrameType(byte: raw[5])
    }
    let flags = raw[6]
    guard flags & 0b1000_0000 == 0 else {
        throw DecodeError.reservedFlagBits(flags: flags)
    }
    guard (flags >> 1) & 0b11 != 0b11 else {
        throw DecodeError.reservedPriorityBits(flags: flags)
    }
    let admissionBits = (flags >> 4) & 0b11
    guard admissionBits != 0b11 else {
        throw DecodeError.reservedAdmissionClass(flags: flags)
    }
    if admissionBits == AdmissionClass.sheddable.rawValue,
       ty != .push, ty != .streamData
    {
        throw DecodeError.sheddableIllegalFrameType(ty: ty, flags: flags)
    }
    if ty.isPureHeader, len != 0 {
        throw DecodeError.pureHeaderFrameWithBody(ty: ty, len: len)
    }

    let channel = UInt16(raw[7]) | (UInt16(raw[8]) << 8)
    let epoch = UInt32(raw[9])
        | (UInt32(raw[10]) << 8)
        | (UInt32(raw[11]) << 16)
        | (UInt32(raw[12]) << 24)
    guard channel != 0 || epoch == 0 else {
        throw DecodeError.nonzeroEpochOnControlChannel(epoch: epoch)
    }
    var corr: UInt64 = 0
    for index in 0..<8 {
        corr |= UInt64(raw[13 + index]) << (8 * UInt64(index))
    }
    return EnvelopeHeader(
        len: len,
        ver: raw[4],
        ty: ty,
        flags: flags,
        channel: channel,
        epoch: epoch,
        corr: corr
    )
}

/// Encode a frame to wire bytes: 21-byte header followed by `len` body bytes.
public func encodeFrame(
    ty: FrameType,
    flags: UInt8,
    channel: UInt16,
    epoch: UInt32,
    corr: UInt64,
    body: Data
) throws -> Data {
    guard body.count <= MAX_FRAME_BODY_LEN,
          let bodyLength = UInt32(exactly: body.count)
    else {
        throw FrameEncodeError.bodyTooLarge(len: body.count, max: MAX_FRAME_BODY_LEN)
    }

    let header = EnvelopeHeader(
        len: bodyLength,
        ver: PROTOCOL_VERSION,
        ty: ty,
        flags: flags,
        channel: channel,
        epoch: epoch,
        corr: corr
    )
    var output = encodeHeader(header)
    output.append(body)
    return output
}
