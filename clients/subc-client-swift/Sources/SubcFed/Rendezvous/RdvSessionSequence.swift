import Foundation

/// A control-session sequence outside the positive 2^53 - 1 domain. Other
/// decimal-string fields retain their own grammars and numeric bounds.
public struct RdvSequenceError: Error, Equatable, Sendable, CustomStringConvertible {
    public let field: String
    public let value: String
    public var description: String { "invalid_sequence: \(field) must be a canonical decimal string in 1...9007199254740991 (received \(value))" }
}

enum RdvSessionSequence {
    static let maximum: UInt64 = 9_007_199_254_740_991

    static func parse(_ value: String, field: String) throws -> UInt64 {
        guard let number = try? RdvDecimalString.parse(value), number > 0, number <= maximum else {
            throw RdvSequenceError(field: field, value: value)
        }
        return number
    }
}
