import Foundation

public typealias FedFrameHeader = FedJSONObject

public enum FedFrameType: String, Sendable, Equatable, CaseIterable {
    case hello
    case bye
    case keepalive
    case catalog
    case call
    case callFrame = "call_frame"
    case callCancel = "call_cancel"
    case effectStatus = "effect_status"
    case effectStatusResult = "effect_status_result"
}

public enum FedFrameError: Error, Equatable, Sendable, CustomStringConvertible {
    case incompleteFrame
    case multipleFrames
    case headerTooLarge(declared: UInt32, maximum: UInt32)
    case bodyTooLarge(declared: UInt32, maximum: UInt32)
    case catalogBodyTooLarge(declared: UInt32, maximum: UInt32)
    case bodylessFrame(type: String, declared: UInt32)
    case invalidHeader(FedJSONError)
    case missingType
    case invalidHeaderField(type: String, field: String)
    case unknownTypeBeforeNegotiation(String)
    case invalidCatalog(FedJSONError)
    case invalidCatalogShape
    case effectResultBodyNotAllowed

    public var description: String {
        switch self {
        case .incompleteFrame: return "incomplete fed frame"
        case .multipleFrames: return "more than one fed frame"
        case .headerTooLarge(let declared, let maximum):
            return "fed frame header length \(declared) exceeds \(maximum)"
        case .bodyTooLarge(let declared, let maximum):
            return "fed frame body length \(declared) exceeds \(maximum)"
        case .catalogBodyTooLarge(let declared, let maximum):
            return "fed catalog body length \(declared) exceeds \(maximum)"
        case .bodylessFrame(let type, let declared):
            return "bodyless fed frame \(type) declared \(declared) bytes"
        case .invalidHeader(let error): return "invalid fed frame header: \(error)"
        case .missingType: return "fed frame header has no string type"
        case .invalidHeaderField(let type, let field):
            return "invalid required field \(field) in fed frame \(type)"
        case .unknownTypeBeforeNegotiation(let type):
            return "unknown fed frame type before negotiation: \(type)"
        case .invalidCatalog(let error): return "invalid fed catalog JSON: \(error)"
        case .invalidCatalogShape: return "invalid fed catalog shape"
        case .effectResultBodyNotAllowed: return "effect_status_result cannot carry this body"
        }
    }
}

public struct FedFrame: Sendable, Equatable {
    public let header: FedJSONObject
    public let body: Data

    public init(header: FedJSONObject, body: Data = Data()) {
        self.header = header
        self.body = body
    }

    public init(type: String, fields: [String: FedJSONValue] = [:], body: Data = Data()) {
        var header = fields
        header["type"] = .string(type)
        self.init(header: FedJSONObject(header), body: body)
    }

    public var typeName: String? {
        guard case .string(let type) = header["type"] else { return nil }
        return type
    }

    public var knownType: FedFrameType? {
        guard let typeName else { return nil }
        return FedFrameType(rawValue: typeName)
    }
}

/// The byte codec for the fed inner frame. It is independent of Noise and can
/// therefore be tested with arbitrary decrypted stream segments.
public enum FedFrameCodec {
    public static let maximumHeaderLength: UInt32 = 65_536
    public static let defaultMaximumBodyLength: UInt32 = 16_777_216
    public static let maximumCatalogBodyLength: UInt32 = 1_048_576

    public static func encode(
        _ frame: FedFrame,
        negotiatedMaximumBodyLength: UInt32 = defaultMaximumBodyLength,
        negotiationComplete: Bool = true,
        negotiatedFeatures: Set<String> = []
    ) throws -> Data {
        try encode(
            header: frame.header,
            body: frame.body,
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiatedFeatures
        )
    }

    public static func encode(
        header: FedJSONObject,
        body: Data = Data(),
        negotiatedMaximumBodyLength: UInt32 = defaultMaximumBodyLength,
        negotiationComplete: Bool = true,
        negotiatedFeatures: Set<String> = []
    ) throws -> Data {
        guard let type = header.typeName else { throw FedFrameError.missingType }
        try validateHeader(
            header,
            type: type,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiatedFeatures
        )
        let headerData: Data
        do {
            headerData = try header.jsonData()
        } catch let error as FedJSONError {
            throw FedFrameError.invalidHeader(error)
        }
        guard headerData.count <= Int(maximumHeaderLength) else {
            throw FedFrameError.headerTooLarge(
                declared: UInt32(min(headerData.count, Int(UInt32.max))),
                maximum: maximumHeaderLength
            )
        }
        let bodyLimit = effectiveBodyLimit(
            for: type,
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength
        )
        guard UInt64(body.count) <= UInt64(bodyLimit) else {
            if type == FedFrameType.catalog.rawValue {
                throw FedFrameError.catalogBodyTooLarge(
                    declared: UInt32(min(body.count, Int(UInt32.max))),
                    maximum: bodyLimit
                )
            }
            throw FedFrameError.bodyTooLarge(
                declared: UInt32(min(body.count, Int(UInt32.max))),
                maximum: bodyLimit
            )
        }
        try validateBody(body, type: type, header: header)

        var result = Data(capacity: 8 + headerData.count + body.count)
        appendUInt32(UInt32(headerData.count), to: &result)
        result.append(headerData)
        appendUInt32(UInt32(body.count), to: &result)
        result.append(body)
        return result
    }

    /// Encode with a caller-supplied JSON header while still applying the
    /// strict object/schema checks. This preserves fixture bytes when a vector
    /// intentionally pins member order or escaping.
    public static func encode(
        headerData: Data,
        body: Data = Data(),
        negotiatedMaximumBodyLength: UInt32 = defaultMaximumBodyLength,
        negotiationComplete: Bool = true,
        negotiatedFeatures: Set<String> = []
    ) throws -> Data {
        let header: FedJSONObject
        do {
            header = try FedJSONObject(jsonData: headerData)
        } catch let error as FedJSONError {
            throw FedFrameError.invalidHeader(error)
        }
        guard let type = header.typeName else { throw FedFrameError.missingType }
        try validateHeader(
            header,
            type: type,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiatedFeatures
        )
        guard headerData.count <= Int(maximumHeaderLength) else {
            throw FedFrameError.headerTooLarge(
                declared: UInt32(min(headerData.count, Int(UInt32.max))),
                maximum: maximumHeaderLength
            )
        }
        let bodyLimit = effectiveBodyLimit(
            for: type,
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength
        )
        guard UInt64(body.count) <= UInt64(bodyLimit) else {
            if type == FedFrameType.catalog.rawValue {
                throw FedFrameError.catalogBodyTooLarge(
                    declared: UInt32(min(body.count, Int(UInt32.max))),
                    maximum: bodyLimit
                )
            }
            throw FedFrameError.bodyTooLarge(
                declared: UInt32(min(body.count, Int(UInt32.max))),
                maximum: bodyLimit
            )
        }
        try validateBody(body, type: type, header: header)
        var result = Data(capacity: 8 + headerData.count + body.count)
        appendUInt32(UInt32(headerData.count), to: &result)
        result.append(headerData)
        appendUInt32(UInt32(body.count), to: &result)
        result.append(body)
        return result
    }

    public static func encode(
        type: String,
        fields: [String: FedJSONValue] = [:],
        body: Data = Data(),
        negotiatedMaximumBodyLength: UInt32 = defaultMaximumBodyLength,
        negotiationComplete: Bool = true,
        negotiatedFeatures: Set<String> = []
    ) throws -> Data {
        try encode(
            FedFrame(type: type, fields: fields, body: body),
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiatedFeatures
        )
    }

    /// Decode exactly one complete frame. For a stream containing multiple
    /// frames use FedFrameStreamDecoder directly or decodeFrames(_:).
    public static func decode(
        _ bytes: Data,
        negotiatedMaximumBodyLength: UInt32 = defaultMaximumBodyLength,
        negotiationComplete: Bool = false,
        negotiatedFeatures: Set<String> = []
    ) throws -> FedFrame {
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiatedFeatures
        )
        let frames = try decoder.append(bytes)
        try decoder.finish()
        guard frames.count == 1 else {
            if frames.isEmpty { throw FedFrameError.incompleteFrame }
            throw FedFrameError.multipleFrames
        }
        return frames[0]
    }

    public static func decodeFrames(
        _ bytes: Data,
        negotiatedMaximumBodyLength: UInt32 = defaultMaximumBodyLength,
        negotiationComplete: Bool = false,
        negotiatedFeatures: Set<String> = []
    ) throws -> [FedFrame] {
        var decoder = FedFrameStreamDecoder(
            negotiatedMaximumBodyLength: negotiatedMaximumBodyLength,
            negotiationComplete: negotiationComplete,
            negotiatedFeatures: negotiatedFeatures
        )
        let frames = try decoder.append(bytes)
        try decoder.finish()
        return frames
    }

    fileprivate static func effectiveBodyLimit(
        for type: String,
        negotiatedMaximumBodyLength: UInt32
    ) -> UInt32 {
        if type == FedFrameType.catalog.rawValue {
            return min(negotiatedMaximumBodyLength, maximumCatalogBodyLength)
        }
        return negotiatedMaximumBodyLength
    }

    fileprivate static func validateHeader(
        _ header: FedJSONObject,
        type: String,
        negotiationComplete: Bool,
        negotiatedFeatures: Set<String>
    ) throws {
        guard let knownType = FedFrameType(rawValue: type) else {
            if !negotiationComplete { throw FedFrameError.unknownTypeBeforeNegotiation(type) }
            return
        }

        switch knownType {
        case .hello:
            try requireArray(header, key: "versions", type: type) { values in
                guard !values.isEmpty, values.count <= 16 else { return false }
                return values.allSatisfy { if case .integer = $0 { return true }; return false }
            }
            try requireArray(header, key: "features", type: type) { values in
                values.count <= 64 && values.allSatisfy { if case .string = $0 { return true }; return false }
            }
            let maxBody = try requireInteger(header, key: "max_body_bytes", type: type)
            guard maxBody >= 4_096, maxBody <= UInt64(UInt32.max) else {
                throw FedFrameError.invalidHeaderField(type: type, field: "max_body_bytes")
            }
            let maxInFlight = try requireInteger(header, key: "max_in_flight", type: type)
            guard (1...4_096).contains(maxInFlight) else {
                throw FedFrameError.invalidHeaderField(type: type, field: "max_in_flight")
            }
            let keepalive = try requireInteger(header, key: "keepalive_interval_ms", type: type)
            guard (1_000...60_000).contains(keepalive) else {
                throw FedFrameError.invalidHeaderField(type: type, field: "keepalive_interval_ms")
            }
            try requireUUID(header, key: "incarnation", type: type)
            try requireNonEmptyString(header, key: "ledger_epoch", type: type)
            let deviceName = try requireString(header, key: "device_name", type: type)
            guard deviceName.data(using: .utf8)?.count ?? Int.max <= 256 else {
                throw FedFrameError.invalidHeaderField(type: type, field: "device_name")
            }
            if let attempt = header["connection_attempt_id"] {
                guard case .string(let value) = attempt,
                      value.count == 32,
                      value.unicodeScalars.allSatisfy({
                          (0x30...0x39).contains($0.value) || (0x61...0x66).contains($0.value)
                      })
                else {
                    throw FedFrameError.invalidHeaderField(type: type, field: "connection_attempt_id")
                }
            }
        case .bye:
            try requireNonEmptyString(header, key: "code", type: type)
        case .keepalive:
            if let watermark = header["confirmed_watermark"] {
                if negotiationComplete && !negotiatedFeatures.contains("effects-v1") {
                    throw FedFrameError.invalidHeaderField(type: type, field: "confirmed_watermark")
                }
                try validateWatermark(watermark, type: type, field: "confirmed_watermark")
            }
        case .catalog:
            _ = try requireInteger(header, key: "generation", type: type)
        case .call:
            try validateEffect(header, type: type)
            try requireNonEmptyString(header, key: "module", type: type)
            if let surface = header["surface"] {
                guard case .string(let surface) = surface,
                      surface == "tool" || surface == "management"
                else { throw FedFrameError.invalidHeaderField(type: type, field: "surface") }
                if surface == "management" && !negotiatedFeatures.contains("mgmt-v1") {
                    throw FedFrameError.invalidHeaderField(type: type, field: "surface")
                }
            }
            if negotiatedFeatures.contains("effects-v1"), negotiationComplete,
               header["mutating"] == nil
            {
                throw FedFrameError.invalidHeaderField(type: type, field: "mutating")
            }
            if let mutating = header["mutating"] {
                guard case .boolean = mutating else {
                    throw FedFrameError.invalidHeaderField(type: type, field: "mutating")
                }
                if case .boolean(true) = mutating,
                   !negotiatedFeatures.contains("effects-v1"),
                   negotiationComplete
                {
                    throw FedFrameError.invalidHeaderField(type: type, field: "mutating")
                }
            }
            if let watermark = header["confirmed_watermark"] {
                if negotiationComplete && !negotiatedFeatures.contains("effects-v1") {
                    throw FedFrameError.invalidHeaderField(type: type, field: "confirmed_watermark")
                }
                try validateWatermark(watermark, type: type, field: "confirmed_watermark")
            }
            try validateDeadline(header, type: type)
        case .callFrame:
            try validateEffect(header, type: type)
            guard let kind = header["k"], case .string(let kind) = kind,
                  ["response", "error", "push", "stream_data", "stream_end"].contains(kind)
            else { throw FedFrameError.invalidHeaderField(type: type, field: "k") }
            try requireBoolean(header, key: "binary", type: type)
            try requireBoolean(header, key: "last", type: type)
            if let omitted = header["body_omitted"] {
                guard case .boolean = omitted else {
                    throw FedFrameError.invalidHeaderField(type: type, field: "body_omitted")
                }
            }
        case .callCancel:
            try validateEffect(header, type: type)
        case .effectStatus:
            if negotiationComplete && !negotiatedFeatures.contains("effects-v1") {
                throw FedFrameError.invalidHeaderField(type: type, field: "type")
            }
            try validateEffect(header, type: type)
        case .effectStatusResult:
            if negotiationComplete && !negotiatedFeatures.contains("effects-v1") {
                throw FedFrameError.invalidHeaderField(type: type, field: "type")
            }
            try validateEffect(header, type: type)
            guard let status = header["status"], case .string(let status) = status,
                  ["recorded", "not_found", "expired"].contains(status)
            else { throw FedFrameError.invalidHeaderField(type: type, field: "status") }
            if let omitted = header["body_omitted"] {
                guard case .boolean = omitted else {
                    throw FedFrameError.invalidHeaderField(type: type, field: "body_omitted")
                }
            }
        }
    }

    fileprivate static func validateBody(_ body: Data, type: String, header: FedJSONObject) throws {
        let bodyless = [
            FedFrameType.hello.rawValue,
            FedFrameType.bye.rawValue,
            FedFrameType.keepalive.rawValue,
            FedFrameType.callCancel.rawValue,
            FedFrameType.effectStatus.rawValue,
        ]
        if bodyless.contains(type), !body.isEmpty {
            throw FedFrameError.bodylessFrame(type: type, declared: UInt32(body.count))
        }
        if type == FedFrameType.catalog.rawValue {
            do {
                let catalog = try FedJSONObject(jsonData: body)
                guard let modules = catalog["modules"],
                      case .array(let values) = modules,
                      values.count <= 64
                else { throw FedFrameError.invalidCatalogShape }
                for value in values {
                    guard case .object(let module) = value else { throw FedFrameError.invalidCatalogShape }
                    if let tools = module["tools"], case .array(let tools) = tools, tools.count > 256 {
                        throw FedFrameError.invalidCatalogShape
                    }
                    if let management = module["management"], case .object(let management) = management,
                       let operations = management["operations"], case .array(let operations) = operations,
                       operations.count > 256
                    {
                        throw FedFrameError.invalidCatalogShape
                    }
                }
            } catch let error as FedFrameError {
                throw error
            } catch let error as FedJSONError {
                throw FedFrameError.invalidCatalog(error)
            }
        }
        if type == FedFrameType.callFrame.rawValue,
           case .boolean(true) = header["body_omitted"],
           !body.isEmpty
        {
            throw FedFrameError.effectResultBodyNotAllowed
        }
        if type == FedFrameType.effectStatusResult.rawValue {
            let status: String? = if case .string(let value) = header["status"] { value } else { nil }
            let omitted = if case .boolean(true) = header["body_omitted"] { true } else { false }
            if status != "recorded" || omitted, !body.isEmpty {
                throw FedFrameError.effectResultBodyNotAllowed
            }
        }
    }

    private static func validateEffect(_ header: FedJSONObject, type: String) throws {
        guard let effect = header["effect"], case .object(let object) = effect else {
            throw FedFrameError.invalidHeaderField(type: type, field: "effect")
        }
        try requireUUID(object, key: "incarnation", type: type)
        _ = try requireInteger(object, key: "seq", type: type)
    }

    private static func validateWatermark(_ value: FedJSONValue, type: String, field: String) throws {
        guard case .object(let object) = value else {
            throw FedFrameError.invalidHeaderField(type: type, field: field)
        }
        try requireUUID(object, key: "incarnation", type: type)
        _ = try requireInteger(object, key: "seq", type: type)
    }

    private static func validateDeadline(_ header: FedJSONObject, type: String) throws {
        let value = try requireInteger(header, key: "deadline_ms", type: type)
        guard (1...3_600_000).contains(value) else {
            throw FedFrameError.invalidHeaderField(type: type, field: "deadline_ms")
        }
    }

    private static func requireString(_ object: FedJSONObject, key: String, type: String) throws -> String {
        guard let value = object[key], case .string(let value) = value else {
            throw FedFrameError.invalidHeaderField(type: type, field: key)
        }
        return value
    }

    private static func requireNonEmptyString(_ object: FedJSONObject, key: String, type: String) throws {
        let value = try requireString(object, key: key, type: type)
        guard !value.isEmpty else {
            throw FedFrameError.invalidHeaderField(type: type, field: key)
        }
    }

    private static func requireUUID(_ object: FedJSONObject, key: String, type: String) throws {
        let value = try requireString(object, key: key, type: type)
        guard let uuid = UUID(uuidString: value), uuid.uuidString.lowercased() == value else {
            throw FedFrameError.invalidHeaderField(type: type, field: key)
        }
    }

    private static func requireInteger(_ object: FedJSONObject, key: String, type: String) throws -> UInt64 {
        guard let value = object[key], case .integer(let value) = value else {
            throw FedFrameError.invalidHeaderField(type: type, field: key)
        }
        return value
    }

    private static func requireBoolean(_ object: FedJSONObject, key: String, type: String) throws {
        guard let value = object[key], case .boolean = value else {
            throw FedFrameError.invalidHeaderField(type: type, field: key)
        }
    }

    private static func requireArray(
        _ object: FedJSONObject,
        key: String,
        type: String,
        predicate: ([FedJSONValue]) -> Bool
    ) throws {
        guard let value = object[key], case .array(let values) = value, predicate(values) else {
            throw FedFrameError.invalidHeaderField(type: type, field: key)
        }
    }

    private static func appendUInt32(_ value: UInt32, to data: inout Data) {
        var littleEndian = value.littleEndian
        withUnsafeBytes(of: &littleEndian) { data.append(contentsOf: $0) }
    }
}

/// Incremental parser for the logical decrypted fed byte stream. It only
/// allocates a header after the bounded header length is known, and only
/// allocates a body after both the negotiated and catalog-specific caps pass.
public struct FedFrameStreamDecoder: Sendable {
    public let negotiatedMaximumBodyLength: UInt32
    public private(set) var negotiationComplete: Bool
    public private(set) var negotiatedFeatures: Set<String>

    private enum Phase: Sendable {
        case headerLength
        case header
        case bodyLength
        case body
    }

    private var phase: Phase = .headerLength
    private var fourBytePrefix = Data()
    private var headerBuffer = Data()
    private var bodyBuffer = Data()
    private var headerLength: Int = 0
    private var bodyLength: Int = 0
    private var bodyLimit: UInt32 = 0
    private var currentHeader: FedJSONObject?
    private var currentType: String?
    private var discardBody = false

    public init(
        negotiatedMaximumBodyLength: UInt32 = FedFrameCodec.defaultMaximumBodyLength,
        negotiationComplete: Bool = false,
        negotiatedFeatures: Set<String> = []
    ) {
        self.negotiatedMaximumBodyLength = negotiatedMaximumBodyLength
        self.negotiationComplete = negotiationComplete
        self.negotiatedFeatures = negotiatedFeatures
    }

    public init(
        maxBodyBytes: UInt32,
        negotiated: Bool = false,
        features: Set<String> = []
    ) {
        self.init(
            negotiatedMaximumBodyLength: maxBodyBytes,
            negotiationComplete: negotiated,
            negotiatedFeatures: features
        )
    }

    public mutating func setNegotiationComplete(_ complete: Bool = true) {
        negotiationComplete = complete
    }

    public mutating func setNegotiation(complete: Bool, features: Set<String>) {
        negotiationComplete = complete
        negotiatedFeatures = features
    }

    public var hasPartialFrame: Bool {
        phase != .headerLength || !fourBytePrefix.isEmpty || !headerBuffer.isEmpty || !bodyBuffer.isEmpty
    }

    public mutating func append(_ bytes: Data) throws -> [FedFrame] {
        if bytes.isEmpty { return [] }
        var frames: [FedFrame] = []
        var offset = 0
        while offset < bytes.count {
            switch phase {
            case .headerLength:
                fourBytePrefix.append(bytes[offset])
                offset += 1
                if fourBytePrefix.count == 4 {
                    let declared = readUInt32(fourBytePrefix)
                    fourBytePrefix.removeAll(keepingCapacity: true)
                    guard declared <= FedFrameCodec.maximumHeaderLength else {
                        throw FedFrameError.headerTooLarge(
                            declared: declared,
                            maximum: FedFrameCodec.maximumHeaderLength
                        )
                    }
                    guard declared > 0 else { throw FedFrameError.invalidHeader(.invalidSyntax) }
                    headerLength = Int(declared)
                    headerBuffer = Data(capacity: headerLength)
                    phase = .header
                }
            case .header:
                let amount = min(headerLength - headerBuffer.count, bytes.count - offset)
                headerBuffer.append(contentsOf: bytes[offset..<(offset + amount)])
                offset += amount
                if headerBuffer.count == headerLength {
                    let header: FedJSONObject
                    do {
                        header = try FedJSONObject(jsonData: headerBuffer)
                    } catch let error as FedJSONError {
                        throw FedFrameError.invalidHeader(error)
                    }
                    guard let type = header.typeName else { throw FedFrameError.missingType }
                    try FedFrameCodec.validateHeader(
                        header,
                        type: type,
                        negotiationComplete: negotiationComplete,
                        negotiatedFeatures: negotiatedFeatures
                    )
                    currentHeader = header
                    currentType = type
                    bodyLimit = FedFrameCodec.effectiveBodyLimit(
                        for: type,
                        negotiatedMaximumBodyLength: negotiatedMaximumBodyLength
                    )
                    phase = .bodyLength
                    fourBytePrefix.removeAll(keepingCapacity: true)
                }
            case .bodyLength:
                fourBytePrefix.append(bytes[offset])
                offset += 1
                if fourBytePrefix.count == 4 {
                    let declared = readUInt32(fourBytePrefix)
                    fourBytePrefix.removeAll(keepingCapacity: true)
                    if currentType == FedFrameType.catalog.rawValue, declared > bodyLimit {
                        throw FedFrameError.catalogBodyTooLarge(declared: declared, maximum: bodyLimit)
                    }
                    guard declared <= negotiatedMaximumBodyLength else {
                        throw FedFrameError.bodyTooLarge(
                            declared: declared,
                            maximum: negotiatedMaximumBodyLength
                        )
                    }
                    guard let type = currentType, let header = currentHeader else {
                        throw FedFrameError.invalidSyntaxInvariant
                    }
                    let bodyless = [
                        FedFrameType.hello.rawValue,
                        FedFrameType.bye.rawValue,
                        FedFrameType.keepalive.rawValue,
                        FedFrameType.callCancel.rawValue,
                        FedFrameType.effectStatus.rawValue,
                    ]
                    if bodyless.contains(type), declared != 0 {
                        throw FedFrameError.bodylessFrame(type: type, declared: declared)
                    }
                    try validateDeclaredBody(declared, type: type, header: header)
                    bodyLength = Int(declared)
                    discardBody = FedFrameType(rawValue: type) == nil
                    bodyBuffer = discardBody ? Data() : Data(capacity: bodyLength)
                    if bodyLength == 0 {
                        try completeBody(&frames)
                    } else {
                        phase = .body
                    }
                }
            case .body:
                let amount = min(bodyLength - bodyBuffer.count, bytes.count - offset)
                if !discardBody {
                    bodyBuffer.append(contentsOf: bytes[offset..<(offset + amount)])
                }
                offset += amount
                if bodyBuffer.count == bodyLength || discardBody && amount > 0 {
                    if discardBody {
                        // For an unknown post-negotiation body, offset is the
                        // only retained state; consume the remaining bytes in
                        // this declaration without allocating them.
                        let remaining = bodyLength - amount
                        let skip = min(remaining, bytes.count - offset)
                        offset += skip
                        bodyLength -= amount + skip
                    }
                    if discardBody && bodyLength > 0 { continue }
                    try completeBody(&frames)
                }
            }
        }
        return frames
    }

    public mutating func finish() throws {
        guard !hasPartialFrame else { throw FedFrameError.incompleteFrame }
    }

    private mutating func completeBody(_ frames: inout [FedFrame]) throws {
        guard let header = currentHeader, let type = currentType else {
            throw FedFrameError.invalidSyntaxInvariant
        }
        if !discardBody {
            try FedFrameCodec.validateBody(bodyBuffer, type: type, header: header)
            frames.append(FedFrame(header: header, body: bodyBuffer))
        }
        phase = .headerLength
        headerBuffer.removeAll(keepingCapacity: true)
        bodyBuffer.removeAll(keepingCapacity: true)
        currentHeader = nil
        currentType = nil
        bodyLength = 0
        headerLength = 0
        bodyLimit = 0
        discardBody = false
    }

    private func validateDeclaredBody(_ declared: UInt32, type: String, header: FedJSONObject) throws {
        if type == FedFrameType.callFrame.rawValue,
           case .boolean(true) = header["body_omitted"], declared != 0
        {
            throw FedFrameError.effectResultBodyNotAllowed
        }
        if type == FedFrameType.effectStatusResult.rawValue {
            let status: String? = if case .string(let value) = header["status"] { value } else { nil }
            let omitted = if case .boolean(true) = header["body_omitted"] { true } else { false }
            if status != "recorded" || omitted, declared != 0 {
                throw FedFrameError.effectResultBodyNotAllowed
            }
        }
    }

    private func readUInt32(_ data: Data) -> UInt32 {
        UInt32(data[data.startIndex])
            | (UInt32(data[data.startIndex + 1]) << 8)
            | (UInt32(data[data.startIndex + 2]) << 16)
            | (UInt32(data[data.startIndex + 3]) << 24)
    }
}

public typealias FedFrameDecoder = FedFrameStreamDecoder
public typealias FedFrameParser = FedFrameStreamDecoder
public typealias FedFrameEncoder = FedFrameCodec

private extension FedJSONObject {
    var typeName: String? {
        guard case .string(let type) = self["type"] else { return nil }
        return type
    }
}

private extension FedFrameError {
    static var invalidSyntaxInvariant: FedFrameError { .invalidHeader(.invalidSyntax) }
}
