import Foundation

/// Errors raised while constructing or parsing values that cross the fed JSON
/// boundary. Fed JSON is deliberately narrower than Foundation's JSON domain:
/// it has no arbitrary objects, non-finite numbers, or unsafe integers.
public enum FedJSONError: Error, Equatable, Sendable, CustomStringConvertible {
    case invalidUTF8
    case invalidSyntax
    case duplicateKey(String)
    case nestingTooDeep(maximum: Int)
    case topLevelMustBeObject
    case negativeInteger
    case unsafeInteger
    case nonFiniteNumber
    case integralNumber
    case unsupportedValue(String)
    case nonStringObjectKey
    case invalidString

    public var description: String {
        switch self {
        case .invalidUTF8: return "fed JSON is not valid UTF-8"
        case .invalidSyntax: return "invalid fed JSON syntax"
        case .duplicateKey(let key): return "duplicate fed JSON object key \(key)"
        case .nestingTooDeep(let maximum): return "fed JSON nesting exceeds \(maximum)"
        case .topLevelMustBeObject: return "fed JSON top level must be an object"
        case .negativeInteger: return "fed JSON integers must be non-negative"
        case .unsafeInteger: return "fed JSON integer is outside the safe range"
        case .nonFiniteNumber: return "fed JSON number is not finite"
        case .integralNumber: return "integral values must use the integer case"
        case .unsupportedValue(let type): return "unsupported worker value \(type)"
        case .nonStringObjectKey: return "fed JSON object keys must be strings"
        case .invalidString: return "invalid fed JSON string"
        }
    }
}

/// The closed JSON value domain used by outbound fed calls and strict fed JSON
/// parsing. Integers are represented as UInt64 so a negative integer cannot be
/// smuggled into a value that is later sent on the wire.
public indirect enum FedJSONValue: Sendable, Equatable, Codable {
    public static let maximumNestingDepth = 128
    public static let firstUnsafeInteger: UInt64 = 9_007_199_254_740_992

    case null
    case boolean(Bool)
    case string(String)
    case integer(UInt64)
    case number(Double)
    case array([FedJSONValue])
    case object(FedJSONObject)

    /// Compatibility spelling for callers that use the shorter Boolean case.
    public static func bool(_ value: Bool) -> FedJSONValue { .boolean(value) }

    /// Compatibility spelling for callers that use `int` for safe integers.
    public static func int(_ value: UInt64) -> FedJSONValue { .integer(value) }

    public init(integer: UInt64) throws {
        guard integer < Self.firstUnsafeInteger else { throw FedJSONError.unsafeInteger }
        self = .integer(integer)
    }

    public init(number: Double) throws {
        guard number.isFinite else { throw FedJSONError.nonFiniteNumber }
        guard number.rounded() != number else { throw FedJSONError.integralNumber }
        self = .number(number)
    }

    public static func fromWorkerValue(_ value: Any) throws -> FedJSONValue {
        try FedJSONValue(any: value)
    }

    public init(any value: Any, depth: Int = 1) throws {
        if value is NSNull {
            self = .null
            return
        }
        if let value = value as? Bool {
            self = .boolean(value)
            return
        }
        if let value = value as? String {
            self = .string(value)
            return
        }
        if let value = value as? NSString {
            self = .string(String(value))
            return
        }

        // Swift's Bool check above is intentional. Foundation bridges Bool to
        // NSNumber, and treating it as 0/1 changes the worker call's meaning.
        if let value = value as? NSNumber {
            if CFGetTypeID(value) == CFBooleanGetTypeID() {
                self = .boolean(value.boolValue)
                return
            }
            try self.init(number: value, depth: depth)
            return
        }

        switch value {
        case let value as Int:
            guard value >= 0 else { throw FedJSONError.negativeInteger }
            try self.init(integer: UInt64(value))
        case let value as Int8:
            guard value >= 0 else { throw FedJSONError.negativeInteger }
            try self.init(integer: UInt64(value))
        case let value as Int16:
            guard value >= 0 else { throw FedJSONError.negativeInteger }
            try self.init(integer: UInt64(value))
        case let value as Int32:
            guard value >= 0 else { throw FedJSONError.negativeInteger }
            try self.init(integer: UInt64(value))
        case let value as Int64:
            guard value >= 0 else { throw FedJSONError.negativeInteger }
            try self.init(integer: UInt64(value))
        case let value as UInt:
            try self.init(integer: UInt64(value))
        case let value as UInt8:
            try self.init(integer: UInt64(value))
        case let value as UInt16:
            try self.init(integer: UInt64(value))
        case let value as UInt32:
            try self.init(integer: UInt64(value))
        case let value as UInt64:
            try self.init(integer: value)
        case let value as Double:
            try self.init(number: value)
        case let value as Float:
            try self.init(number: Double(value))
        case let value as Decimal:
            try self.init(number: NSDecimalNumber(decimal: value).doubleValue)
        case let value as [Any]:
            guard depth <= Self.maximumNestingDepth else {
                throw FedJSONError.nestingTooDeep(maximum: Self.maximumNestingDepth)
            }
            self = .array(try value.map { try FedJSONValue(any: $0, depth: depth + 1) })
        case let value as NSArray:
            guard depth <= Self.maximumNestingDepth else {
                throw FedJSONError.nestingTooDeep(maximum: Self.maximumNestingDepth)
            }
            self = .array(try value.map { try FedJSONValue(any: $0, depth: depth + 1) })
        case let value as NSDictionary:
            self = .object(try FedJSONObject(snapshotting: value, depth: depth))
        default:
            throw FedJSONError.unsupportedValue(String(describing: type(of: value)))
        }
    }

    private init(number value: NSNumber, depth: Int) throws {
        let doubleValue = value.doubleValue
        guard doubleValue.isFinite else { throw FedJSONError.nonFiniteNumber }
        if doubleValue.rounded() == doubleValue {
            guard doubleValue >= 0,
                  doubleValue < Double(Self.firstUnsafeInteger),
                  let integer = UInt64(exactly: doubleValue)
            else {
                if doubleValue < 0 { throw FedJSONError.negativeInteger }
                throw FedJSONError.unsafeInteger
            }
            try self.init(integer: integer)
        } else {
            self = .number(doubleValue)
        }
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() {
            self = .null
        } else if let value = try? container.decode(Bool.self) {
            self = .boolean(value)
        } else if let value = try? container.decode(UInt64.self) {
            try self.init(integer: value)
        } else if let value = try? container.decode(Double.self) {
            try self.init(number: value)
        } else if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode([FedJSONValue].self) {
            self = .array(value)
            try validate()
        } else if let value = try? container.decode(FedJSONObject.self) {
            self = .object(value)
            try validate()
        } else {
            throw FedJSONError.invalidSyntax
        }
    }

    public func encode(to encoder: Encoder) throws {
        try validate()
        var container = encoder.singleValueContainer()
        switch self {
        case .null:
            try container.encodeNil()
        case .boolean(let value):
            try container.encode(value)
        case .string(let value):
            try container.encode(value)
        case .integer(let value):
            try container.encode(value)
        case .number(let value):
            try container.encode(value)
        case .array(let value):
            try container.encode(value)
        case .object(let value):
            try container.encode(value)
        }
    }

    /// Parse one strict fed JSON document. This method accepts any JSON value;
    /// use FedJSONObject.init(jsonData:) when an object top level is required.
    public static func parse(_ data: Data) throws -> FedJSONValue {
        var parser = try FedStrictJSONParser(data: data)
        return try parser.parse()
    }

    public init(jsonData data: Data) throws {
        self = try Self.parse(data)
    }

    public func jsonData() throws -> Data {
        try validate()
        return Data(FedJSONWriter().write(self).utf8)
    }

    fileprivate func validate(depth: Int = 1) throws {
        switch self {
        case .null, .boolean, .string:
            return
        case .integer(let value):
            guard value < Self.firstUnsafeInteger else { throw FedJSONError.unsafeInteger }
        case .number(let value):
            guard value.isFinite else { throw FedJSONError.nonFiniteNumber }
            guard value.rounded() != value else { throw FedJSONError.integralNumber }
        case .array(let values):
            guard depth <= Self.maximumNestingDepth else {
                throw FedJSONError.nestingTooDeep(maximum: Self.maximumNestingDepth)
            }
            for value in values { try value.validate(depth: depth + 1) }
        case .object(let object):
            try object.validate(depth: depth)
        }
    }

    fileprivate func writeJSON() throws -> String {
        try validate()
        return FedJSONWriter().write(self)
    }
}

/// An immutable, string-keyed JSON object. The dictionary is copied on input
/// and only read access is exposed, so the object can safely cross actors.
public struct FedJSONObject: Sendable, Equatable, Codable, ExpressibleByDictionaryLiteral {
    private let storage: [String: FedJSONValue]

    public init(_ values: [String: FedJSONValue] = [:]) {
        storage = values
    }

    public init(dictionaryLiteral elements: (String, FedJSONValue)...) {
        var values: [String: FedJSONValue] = [:]
        for (key, value) in elements {
            // A Swift dictionary literal cannot retain duplicate keys. The
            // strict parser and worker snapshotter reject duplicates before this
            // initializer is reached.
            values[key] = value
        }
        storage = values
    }

    public init(validating values: [String: FedJSONValue]) throws {
        storage = values
        try validate()
    }

    public init(jsonData data: Data) throws {
        let value = try FedJSONValue.parse(data)
        guard case .object(let object) = value else { throw FedJSONError.topLevelMustBeObject }
        self = object
        try validate()
    }

    public init(any value: Any) throws {
        guard let object = try FedJSONValue(any: value) as FedJSONValue?,
              case .object(let object) = object
        else {
            throw FedJSONError.topLevelMustBeObject
        }
        self = object
        try validate()
    }

    public static func snapshot(_ params: [String: Any]) throws -> FedJSONObject {
        try FedJSONObject(any: params)
    }

    public static func fromWorkerParams(_ params: [String: Any]) throws -> FedJSONObject {
        try snapshot(params)
    }

    public static func fromWorkerDictionary(_ params: [String: Any]) throws -> FedJSONObject {
        try snapshot(params)
    }

    public var count: Int { storage.count }
    public var isEmpty: Bool { storage.isEmpty }
    public var keys: Dictionary<String, FedJSONValue>.Keys { storage.keys }
    public var dictionary: [String: FedJSONValue] { storage }

    public subscript(key: String) -> FedJSONValue? { storage[key] }

    public func jsonData() throws -> Data {
        try validate()
        return Data(FedJSONWriter().write(.object(self)).utf8)
    }

    public func encode(to encoder: Encoder) throws {
        try validate()
        var container = encoder.container(keyedBy: DynamicCodingKey.self)
        for key in storage.keys.sorted() {
            guard let value = storage[key] else { continue }
            try container.encode(value, forKey: DynamicCodingKey(stringValue: key))
        }
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: DynamicCodingKey.self)
        var values: [String: FedJSONValue] = [:]
        for key in container.allKeys {
            values[key.stringValue] = try container.decode(FedJSONValue.self, forKey: key)
        }
        try self.init(validating: values)
    }

    fileprivate init(snapshotting dictionary: NSDictionary, depth: Int) throws {
        guard depth <= FedJSONValue.maximumNestingDepth else {
            throw FedJSONError.nestingTooDeep(maximum: FedJSONValue.maximumNestingDepth)
        }
        var values: [String: FedJSONValue] = [:]
        for (key, rawValue) in dictionary {
            guard let key = key as? String else { throw FedJSONError.nonStringObjectKey }
            if values[key] != nil { throw FedJSONError.duplicateKey(key) }
            values[key] = try FedJSONValue(any: rawValue, depth: depth + 1)
        }
        storage = values
        try validate(depth: depth)
    }

    fileprivate func validate(depth: Int = 1) throws {
        guard depth <= FedJSONValue.maximumNestingDepth else {
            throw FedJSONError.nestingTooDeep(maximum: FedJSONValue.maximumNestingDepth)
        }
        for (key, value) in storage {
            guard !key.contains("\0") else { throw FedJSONError.invalidString }
            try value.validate(depth: depth + 1)
        }
    }
}

public enum FedWorkerJSONConverter {
    public static func snapshot(_ value: Any) throws -> FedJSONValue {
        try FedJSONValue(any: value)
    }

    public static func snapshotObject(_ value: [String: Any]) throws -> FedJSONObject {
        try FedJSONObject.snapshot(value)
    }

    public static func encodeManagementCall(method: String, params: [String: Any]) throws -> Data {
        try FedManagementCallBody.encode(method: method, workerParams: params)
    }
}

public struct FedManagementCallBody: Sendable, Equatable {
    public let method: String
    public let params: FedJSONObject

    public init(method: String, params: FedJSONObject) {
        self.method = method
        self.params = params
    }

    public init(method: String, workerParams: [String: Any]) throws {
        self.method = method
        self.params = try FedJSONObject.snapshot(workerParams)
    }

    public func jsonData() throws -> Data {
        let body = FedJSONObject([
            "method": .string(method),
            "params": .object(params),
        ])
        return try body.jsonData()
    }

    public static func encode(method: String, params: FedJSONObject) throws -> Data {
        try Self(method: method, params: params).jsonData()
    }

    public static func encode(method: String, workerParams: [String: Any]) throws -> Data {
        try Self(method: method, workerParams: workerParams).jsonData()
    }
}

private struct DynamicCodingKey: CodingKey, Hashable {
    let stringValue: String
    init(stringValue: String) { self.stringValue = stringValue }
    let intValue: Int? = nil
    init?(intValue: Int) { return nil }
}

private struct FedJSONWriter {
    func write(_ value: FedJSONValue) -> String {
        switch value {
        case .null: return "null"
        case .boolean(let value): return value ? "true" : "false"
        case .string(let value): return quote(value)
        case .integer(let value): return String(value)
        case .number(let value): return String(value)
        case .array(let values):
            return "[" + values.map(write).joined(separator: ",") + "]"
        case .object(let object):
            return "{" + object.dictionary.keys.sorted().compactMap { key in
                guard let value = object.dictionary[key] else { return nil }
                return quote(key) + ":" + write(value)
            }.joined(separator: ",") + "}"
        }
    }

    private func quote(_ value: String) -> String {
        var result = "\""
        result.reserveCapacity(value.utf8.count + 2)
        for scalar in value.unicodeScalars {
            switch scalar.value {
            case 0x22: result += "\\\""
            case 0x5C: result += "\\\\"
            case 0x08: result += "\\b"
            case 0x0C: result += "\\f"
            case 0x0A: result += "\\n"
            case 0x0D: result += "\\r"
            case 0x09: result += "\\t"
            case 0..<0x20:
                result += String(format: "\\u%04x", scalar.value)
            default:
                result.unicodeScalars.append(scalar)
            }
        }
        result += "\""
        return result
    }
}

private struct FedStrictJSONParser {
    private let bytes: [UInt8]
    private var index = 0

    init(data: Data) throws {
        // Checking through String first makes malformed UTF-8 fail before the
        // parser can interpret any byte as syntax.
        guard String(data: data, encoding: .utf8) != nil else { throw FedJSONError.invalidUTF8 }
        bytes = Array(data)
    }

    mutating func parse() throws -> FedJSONValue {
        skipWhitespace()
        let value = try parseValue(depth: 1)
        skipWhitespace()
        guard index == bytes.count else { throw FedJSONError.invalidSyntax }
        return value
    }

    private mutating func parseValue(depth: Int) throws -> FedJSONValue {
        guard let byte = peek else { throw FedJSONError.invalidSyntax }
        switch byte {
        case 0x6E: try consumeLiteral("null"); return .null
        case 0x74: try consumeLiteral("true"); return .boolean(true)
        case 0x66: try consumeLiteral("false"); return .boolean(false)
        case 0x22: return .string(try parseString())
        case 0x5B:
            guard depth <= FedJSONValue.maximumNestingDepth else {
                throw FedJSONError.nestingTooDeep(maximum: FedJSONValue.maximumNestingDepth)
            }
            return .array(try parseArray(depth: depth))
        case 0x7B:
            guard depth <= FedJSONValue.maximumNestingDepth else {
                throw FedJSONError.nestingTooDeep(maximum: FedJSONValue.maximumNestingDepth)
            }
            return .object(try parseObject(depth: depth))
        case 0x2D, 0x30...0x39: return try parseNumber()
        default: throw FedJSONError.invalidSyntax
        }
    }

    private mutating func parseArray(depth: Int) throws -> [FedJSONValue] {
        try consume(0x5B)
        skipWhitespace()
        if consumeIfPresent(0x5D) { return [] }
        var values: [FedJSONValue] = []
        while true {
            values.append(try parseValue(depth: depth + 1))
            skipWhitespace()
            if consumeIfPresent(0x5D) { return values }
            try consume(0x2C)
            skipWhitespace()
        }
    }

    private mutating func parseObject(depth: Int) throws -> FedJSONObject {
        try consume(0x7B)
        skipWhitespace()
        var values: [String: FedJSONValue] = [:]
        if consumeIfPresent(0x7D) { return FedJSONObject(values) }
        while true {
            guard peek == 0x22 else { throw FedJSONError.invalidSyntax }
            let key = try parseString()
            if values[key] != nil { throw FedJSONError.duplicateKey(key) }
            skipWhitespace()
            try consume(0x3A)
            skipWhitespace()
            values[key] = try parseValue(depth: depth + 1)
            skipWhitespace()
            if consumeIfPresent(0x7D) { return FedJSONObject(values) }
            try consume(0x2C)
            skipWhitespace()
        }
    }

    private mutating func parseString() throws -> String {
        try consume(0x22)
        var scalars: [UnicodeScalar] = []
        var rawBytes: [UInt8] = []

        func flush(_ bytes: inout [UInt8], into scalars: inout [UnicodeScalar]) throws {
            guard !bytes.isEmpty else { return }
            guard let string = String(bytes: bytes, encoding: .utf8) else {
                throw FedJSONError.invalidUTF8
            }
            scalars.append(contentsOf: string.unicodeScalars)
            bytes.removeAll(keepingCapacity: true)
        }

        while let byte = peek {
            index += 1
            if byte == 0x22 {
                try flush(&rawBytes, into: &scalars)
                return String(String.UnicodeScalarView(scalars))
            }
            if byte < 0x20 { throw FedJSONError.invalidString }
            if byte != 0x5C {
                rawBytes.append(byte)
                continue
            }
            try flush(&rawBytes, into: &scalars)
            guard let escape = peek else { throw FedJSONError.invalidSyntax }
            index += 1
            switch escape {
            case 0x22: scalars.append("\"")
            case 0x5C: scalars.append("\\")
            case 0x2F: scalars.append("/")
            case 0x62: scalars.append("\u{08}")
            case 0x66: scalars.append("\u{0C}")
            case 0x6E: scalars.append("\n")
            case 0x72: scalars.append("\r")
            case 0x74: scalars.append("\t")
            case 0x75:
                let first = try parseHexQuad()
                if (0xD800...0xDBFF).contains(first) {
                    guard consumeIfPresent(0x5C), consumeIfPresent(0x75) else {
                        throw FedJSONError.invalidString
                    }
                    let second = try parseHexQuad()
                    guard (0xDC00...0xDFFF).contains(second) else {
                        throw FedJSONError.invalidString
                    }
                    let scalarValue = 0x10000 + ((UInt32(first) - 0xD800) << 10) + (UInt32(second) - 0xDC00)
                    guard let scalar = UnicodeScalar(scalarValue) else { throw FedJSONError.invalidString }
                    scalars.append(scalar)
                } else {
                    guard !(0xDC00...0xDFFF).contains(first),
                          let scalar = UnicodeScalar(UInt32(first))
                    else { throw FedJSONError.invalidString }
                    scalars.append(scalar)
                }
            default:
                throw FedJSONError.invalidString
            }
        }
        throw FedJSONError.invalidSyntax
    }

    private mutating func parseHexQuad() throws -> UInt16 {
        guard index + 4 <= bytes.count else { throw FedJSONError.invalidSyntax }
        var value: UInt16 = 0
        for _ in 0..<4 {
            guard let digit = hexValue(bytes[index]) else { throw FedJSONError.invalidString }
            value = (value << 4) | UInt16(digit)
            index += 1
        }
        return value
    }

    private mutating func parseNumber() throws -> FedJSONValue {
        let start = index
        if consumeIfPresent(0x2D) {}
        if consumeIfPresent(0x30) {
            if let next = peek, (0x30...0x39).contains(next) { throw FedJSONError.invalidSyntax }
        } else {
            guard consumeDigits(minimum: 1) else { throw FedJSONError.invalidSyntax }
        }
        var hasFraction = false
        if consumeIfPresent(0x2E) {
            hasFraction = true
            guard consumeDigits(minimum: 1) else { throw FedJSONError.invalidSyntax }
        }
        var hasExponent = false
        if let next = peek, next == 0x65 || next == 0x45 {
            hasExponent = true
            index += 1
            if peek == 0x2B || peek == 0x2D { index += 1 }
            guard consumeDigits(minimum: 1) else { throw FedJSONError.invalidSyntax }
        }
        let token = String(decoding: bytes[start..<index], as: UTF8.self)
        guard let value = Double(token), value.isFinite else { throw FedJSONError.nonFiniteNumber }
        if hasFraction || hasExponent {
            guard value.rounded() != value else { throw FedJSONError.integralNumber }
            return .number(value)
        }
        guard !token.hasPrefix("-"), value >= 0,
              value < Double(FedJSONValue.firstUnsafeInteger),
              let integer = UInt64(exactly: value)
        else {
            if token.hasPrefix("-") { throw FedJSONError.negativeInteger }
            throw FedJSONError.unsafeInteger
        }
        return .integer(integer)
    }

    private mutating func consumeDigits(minimum: Int) -> Bool {
        let start = index
        while let byte = peek, (0x30...0x39).contains(byte) { index += 1 }
        return index - start >= minimum
    }

    private mutating func consumeLiteral(_ literal: String) throws {
        let literalBytes = Array(literal.utf8)
        guard index + literalBytes.count <= bytes.count,
              Array(bytes[index..<(index + literalBytes.count)]) == literalBytes
        else { throw FedJSONError.invalidSyntax }
        index += literalBytes.count
    }

    private mutating func consume(_ byte: UInt8) throws {
        guard consumeIfPresent(byte) else { throw FedJSONError.invalidSyntax }
    }

    private mutating func consumeIfPresent(_ byte: UInt8) -> Bool {
        guard peek == byte else { return false }
        index += 1
        return true
    }

    private mutating func skipWhitespace() {
        while let byte = peek, byte == 0x20 || byte == 0x09 || byte == 0x0A || byte == 0x0D {
            index += 1
        }
    }

    private var peek: UInt8? { index < bytes.count ? bytes[index] : nil }

    private func hexValue(_ byte: UInt8) -> UInt8? {
        switch byte {
        case 0x30...0x39: return byte - 0x30
        case 0x41...0x46: return byte - 0x41 + 10
        case 0x61...0x66: return byte - 0x61 + 10
        default: return nil
        }
    }
}
