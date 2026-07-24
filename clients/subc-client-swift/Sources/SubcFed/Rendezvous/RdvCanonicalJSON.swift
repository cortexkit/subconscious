import Foundation

/// Errors raised by the rdv-wire canonical JSON discipline (docs/rdv-wire.md
/// §1.2). This is deliberately stricter than the fed-management JSON domain:
/// signed payloads carry no JSON number literals, require NFC strings, and
/// admit only the minimal string escapes, so a byte-identical canonical form
/// is reachable from both the TypeScript and Rust implementations.
public enum RdvJSONError: Error, Equatable, Sendable, CustomStringConvertible {
    case invalidUTF8
    case invalidSyntax
    case invalidString
    case duplicateKey(String)
    case nestingTooDeep(maximum: Int)
    case numberLiteral
    case nonNFCString
    case nonMinimalEscape
    case unknownField(String)
    case missingField(String)
    case wrongType(field: String)
    case unsupportedValue(String)
    case topLevelMustBeObject
    case invalidDecimalString(String)

    public var description: String {
        switch self {
        case .invalidUTF8: return "rdv-wire JSON is not valid UTF-8"
        case .invalidSyntax: return "invalid rdv-wire JSON syntax"
        case .invalidString: return "invalid rdv-wire JSON string"
        case .duplicateKey(let key): return "duplicate rdv-wire JSON object key \(key)"
        case .nestingTooDeep(let maximum): return "rdv-wire JSON nesting exceeds \(maximum) containers"
        case .numberLiteral: return "rdv-wire signed payloads carry no JSON number literals"
        case .nonNFCString: return "rdv-wire JSON string is not NFC-normalized"
        case .nonMinimalEscape: return "rdv-wire JSON string uses a non-minimal escape"
        case .unknownField(let field): return "unknown rdv-wire field \(field)"
        case .missingField(let field): return "missing rdv-wire field \(field)"
        case .wrongType(let field): return "rdv-wire field \(field) has the wrong type"
        case .unsupportedValue(let type): return "unsupported rdv-wire value \(type)"
        case .topLevelMustBeObject: return "rdv-wire top level must be an object"
        case .invalidDecimalString(let value): return "invalid rdv-wire decimal string \(value)"
        }
    }
}

/// The closed JSON value domain used by rdv-wire signed payloads and strict
/// parsing. There is intentionally NO number case: every numeric quantity on
/// the rdv-wire is a decimal string (§1.2), so a JSON number literal can never
/// enter a value that is later canonicalized for signing.
public indirect enum RdvJSONValue: Sendable, Equatable {
    case null
    case boolean(Bool)
    case string(String)
    case array([RdvJSONValue])
    case object(RdvJSONObject)

    /// Parse one strict rdv-wire JSON document (any top-level value).
    public static func parse(_ data: Data) throws -> RdvJSONValue {
        var parser = try RdvStrictJSONParser(data: data)
        return try parser.parse()
    }

    /// Parse a strict rdv-wire JSON document that must be a top-level object.
    public static func parseObject(_ data: Data) throws -> RdvJSONObject {
        guard case .object(let object) = try parse(data) else {
            throw RdvJSONError.topLevelMustBeObject
        }
        return object
    }

    /// Convert a Foundation-parsed JSON fragment (from JSONSerialization) into
    /// the rdv-wire domain. JSON number literals are rejected: rdv-wire numerics
    /// are decimal strings, so a number arriving here is a contract violation.
    public init(any value: Any) throws {
        if value is NSNull {
            self = .null
            return
        }
        // Check NSNumber before String/collections and distinguish real booleans
        // from numeric 0/1: JSONSerialization bridges true/false to a CFBoolean
        // NSNumber, which `as? Bool` would also accept for a numeric 1.
        if let value = value as? NSNumber {
            if CFGetTypeID(value) == CFBooleanGetTypeID() {
                self = .boolean(value.boolValue)
                return
            }
            throw RdvJSONError.numberLiteral
        }
        if let value = value as? String {
            self = .string(value)
            return
        }
        if let value = value as? [Any] {
            self = .array(try value.map { try RdvJSONValue(any: $0) })
            return
        }
        if let value = value as? [String: Any] {
            var storage: [String: RdvJSONValue] = [:]
            for (key, element) in value {
                storage[key] = try RdvJSONValue(any: element)
            }
            self = .object(RdvJSONObject(storage))
            return
        }
        throw RdvJSONError.unsupportedValue(String(describing: type(of: value)))
    }
}

/// An immutable, string-keyed rdv-wire JSON object. Key order is irrelevant to
/// equality and to canonicalization (which sorts keys byte-lexicographically),
/// so a plain dictionary backs it.
public struct RdvJSONObject: Sendable, Equatable {
    public let storage: [String: RdvJSONValue]

    public init(_ storage: [String: RdvJSONValue] = [:]) {
        self.storage = storage
    }

    public subscript(key: String) -> RdvJSONValue? { storage[key] }
    public var keys: [String] { Array(storage.keys) }
    public var isEmpty: Bool { storage.isEmpty }
    public var count: Int { storage.count }
}

/// The rdv-wire canonical serializer (docs/rdv-wire.md §1.2). Output is
/// byte-identical across implementations: object keys sorted byte-
/// lexicographically at every depth, no insignificant whitespace, the two
/// mandatory escapes plus `\u00XX` (lowercase hex) for control characters
/// U+0000–U+001F, every other character literal, and NFC strings only.
public enum RdvCanonicalJSON {
    public static func canonicalize(_ value: RdvJSONValue) throws -> Data {
        var output = Data()
        try write(value, into: &output)
        return output
    }

    public static func canonicalString(_ value: RdvJSONValue) throws -> String {
        String(decoding: try canonicalize(value), as: UTF8.self)
    }

    private static func write(_ value: RdvJSONValue, into output: inout Data) throws {
        switch value {
        case .null:
            output.append(contentsOf: [0x6E, 0x75, 0x6C, 0x6C]) // null
        case .boolean(let value):
            output.append(contentsOf: value
                ? [0x74, 0x72, 0x75, 0x65] // true
                : [0x66, 0x61, 0x6C, 0x73, 0x65]) // false
        case .string(let value):
            try writeString(value, into: &output)
        case .array(let values):
            output.append(0x5B) // [
            for (index, element) in values.enumerated() {
                if index > 0 { output.append(0x2C) } // ,
                try write(element, into: &output)
            }
            output.append(0x5D) // ]
        case .object(let object):
            output.append(0x7B) // {
            let keys = object.storage.keys.sorted {
                Array($0.utf8).lexicographicallyPrecedes(Array($1.utf8))
            }
            for (index, key) in keys.enumerated() {
                if index > 0 { output.append(0x2C) } // ,
                try writeString(key, into: &output)
                output.append(0x3A) // :
                try write(object.storage[key]!, into: &output)
            }
            output.append(0x7D) // }
        }
    }

    private static func writeString(_ value: String, into output: inout Data) throws {
        guard isNFC(value) else { throw RdvJSONError.nonNFCString }
        output.append(0x22) // "
        for scalar in value.unicodeScalars {
            switch scalar.value {
            case 0x22:
                output.append(contentsOf: [0x5C, 0x22]) // \"
            case 0x5C:
                output.append(contentsOf: [0x5C, 0x5C]) // \\
            case 0x00...0x1F:
                // Control characters use the lowercase \u00XX escape; the short
                // forms (\t, \n, ...) are non-minimal and never emitted.
                let escape = String(format: "\\u%04x", scalar.value)
                output.append(contentsOf: Array(escape.utf8))
            default:
                output.append(contentsOf: Array(String(scalar).utf8))
            }
        }
        output.append(0x22) // "
    }

    /// NFC check (not normalization): the bytes on the wire are the canon, so a
    /// non-NFC string is invalid rather than re-normalized. `precomposedString
    /// WithCanonicalMapping` is Foundation's NFC form. Comparison is byte-level
    /// (not Swift String `==`, which equates a decomposed sequence with its
    /// precomposed grapheme cluster), so a decomposed string is correctly seen as
    /// differing from its NFC bytes.
    static func isNFC(_ value: String) -> Bool {
        let nfc = (value as NSString).precomposedStringWithCanonicalMapping
        return Array(value.utf8) == Array(nfc.utf8)
    }
}

/// The strict rdv-wire JSON parser. Rejects everything the canonical form
/// forbids on the way in: JSON number literals, duplicate keys, non-NFC
/// strings, non-minimal escapes (solidus, short control forms, uppercase hex,
/// `\u` for non-control characters), and nesting beyond 128 total containers.
public struct RdvStrictJSONParser {
    /// Maximum total number of nested containers (objects + arrays). The
    /// cross-language vectors pin 128: a root object plus 127 arrays is
    /// accepted, the 129th container is rejected.
    public static let maximumContainers = 128

    private let bytes: [UInt8]
    private var index = 0

    public init(data: Data) throws {
        // Validating through String first makes malformed UTF-8 fail before the
        // parser can interpret a stray byte as syntax.
        guard String(data: data, encoding: .utf8) != nil else { throw RdvJSONError.invalidUTF8 }
        bytes = Array(data)
    }

    public mutating func parse() throws -> RdvJSONValue {
        skipWhitespace()
        let value = try parseValue(depth: 1)
        skipWhitespace()
        guard index == bytes.count else { throw RdvJSONError.invalidSyntax }
        return value
    }

    private mutating func parseValue(depth: Int) throws -> RdvJSONValue {
        guard let byte = peek else { throw RdvJSONError.invalidSyntax }
        switch byte {
        case 0x6E: try consumeLiteral("null"); return .null
        case 0x74: try consumeLiteral("true"); return .boolean(true)
        case 0x66: try consumeLiteral("false"); return .boolean(false)
        case 0x22: return .string(try parseString())
        case 0x5B:
            guard depth <= Self.maximumContainers else {
                throw RdvJSONError.nestingTooDeep(maximum: Self.maximumContainers)
            }
            return .array(try parseArray(depth: depth))
        case 0x7B:
            guard depth <= Self.maximumContainers else {
                throw RdvJSONError.nestingTooDeep(maximum: Self.maximumContainers)
            }
            return .object(try parseObject(depth: depth))
        case 0x2D, 0x30...0x39:
            // A JSON number literal anywhere in rdv-wire is a contract violation:
            // numerics are decimal strings (§1.2).
            throw RdvJSONError.numberLiteral
        default:
            throw RdvJSONError.invalidSyntax
        }
    }

    private mutating func parseArray(depth: Int) throws -> [RdvJSONValue] {
        try consume(0x5B)
        skipWhitespace()
        if consumeIfPresent(0x5D) { return [] }
        var values: [RdvJSONValue] = []
        while true {
            values.append(try parseValue(depth: depth + 1))
            skipWhitespace()
            if consumeIfPresent(0x5D) { return values }
            try consume(0x2C)
            skipWhitespace()
        }
    }

    private mutating func parseObject(depth: Int) throws -> RdvJSONObject {
        try consume(0x7B)
        skipWhitespace()
        var storage: [String: RdvJSONValue] = [:]
        if consumeIfPresent(0x7D) { return RdvJSONObject(storage) }
        while true {
            guard peek == 0x22 else { throw RdvJSONError.invalidSyntax }
            let key = try parseString()
            if storage[key] != nil { throw RdvJSONError.duplicateKey(key) }
            skipWhitespace()
            try consume(0x3A)
            skipWhitespace()
            storage[key] = try parseValue(depth: depth + 1)
            skipWhitespace()
            if consumeIfPresent(0x7D) { return RdvJSONObject(storage) }
            try consume(0x2C)
            skipWhitespace()
        }
    }

    private mutating func parseString() throws -> String {
        try consume(0x22)
        var scalars: [UnicodeScalar] = []
        var rawBytes: [UInt8] = []

        func flush() throws {
            guard !rawBytes.isEmpty else { return }
            guard let string = String(bytes: rawBytes, encoding: .utf8) else {
                throw RdvJSONError.invalidUTF8
            }
            scalars.append(contentsOf: string.unicodeScalars)
            rawBytes.removeAll(keepingCapacity: true)
        }

        while let byte = peek {
            index += 1
            if byte == 0x22 {
                try flush()
                let result = String(String.UnicodeScalarView(scalars))
                guard RdvCanonicalJSON.isNFC(result) else { throw RdvJSONError.nonNFCString }
                return result
            }
            // A raw control byte inside a string is invalid; control characters
            // must arrive as the \u00XX escape.
            if byte < 0x20 { throw RdvJSONError.invalidString }
            if byte != 0x5C {
                rawBytes.append(byte)
                continue
            }
            try flush()
            guard let escape = peek else { throw RdvJSONError.invalidSyntax }
            index += 1
            switch escape {
            case 0x22: scalars.append("\"") // \"
            case 0x5C: scalars.append("\\") // \\
            case 0x75: // \u — only lowercase-hex control characters U+0000–U+001F
                let value = try parseLowercaseHexQuad()
                guard value <= 0x1F else { throw RdvJSONError.nonMinimalEscape }
                guard let scalar = UnicodeScalar(UInt32(value)) else { throw RdvJSONError.invalidString }
                scalars.append(scalar)
            default:
                // Rejects \/ (solidus), the short control forms (\b \f \n \r \t),
                // and any other escape: only \", \\, and \u00XX are minimal.
                throw RdvJSONError.nonMinimalEscape
            }
        }
        throw RdvJSONError.invalidSyntax
    }

    private mutating func parseLowercaseHexQuad() throws -> UInt16 {
        guard index + 4 <= bytes.count else { throw RdvJSONError.invalidSyntax }
        var value: UInt16 = 0
        for _ in 0..<4 {
            let byte = bytes[index]
            let nibble: UInt16
            switch byte {
            case 0x30...0x39: nibble = UInt16(byte - 0x30)
            case 0x61...0x66: nibble = UInt16(byte - 0x61 + 10) // lowercase only
            default: throw RdvJSONError.nonMinimalEscape // uppercase hex is non-minimal
            }
            value = (value << 4) | nibble
            index += 1
        }
        return value
    }

    private mutating func consumeLiteral(_ literal: String) throws {
        let literalBytes = Array(literal.utf8)
        guard index + literalBytes.count <= bytes.count,
              Array(bytes[index..<(index + literalBytes.count)]) == literalBytes
        else { throw RdvJSONError.invalidSyntax }
        index += literalBytes.count
    }

    private mutating func consume(_ byte: UInt8) throws {
        guard consumeIfPresent(byte) else { throw RdvJSONError.invalidSyntax }
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
}

/// rdv-wire decimal-string numeric discipline (§1.2). A decimal string is the
/// canonical text of a non-negative integer: "0", or a non-zero digit followed
/// by digits. No sign, no leading zeros, no empty string, digits only.
public enum RdvDecimalString {
    public static func isValid(_ value: String) -> Bool {
        let bytes = Array(value.utf8)
        guard !bytes.isEmpty else { return false }
        guard bytes.allSatisfy({ (0x30...0x39).contains($0) }) else { return false }
        if bytes.count == 1 { return true }
        return bytes[0] != 0x30 // multi-digit values carry no leading zero
    }

    /// Parse a decimal string into an unsigned integer, rejecting any value that
    /// is not canonical decimal-string text.
    public static func parse(_ value: String) throws -> UInt64 {
        guard isValid(value) else { throw RdvJSONError.invalidDecimalString(value) }
        guard let parsed = UInt64(value) else { throw RdvJSONError.invalidDecimalString(value) }
        return parsed
    }
}
