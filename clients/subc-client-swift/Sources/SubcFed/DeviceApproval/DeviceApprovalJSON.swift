import Foundation

// JSONSerialization validates JSON syntax but discards duplicate members. Scan object keys
// before using it so a second spelling (including an escaped spelling) cannot overwrite a value.
struct ApprovalJSONScanner {
    let bytes: [UInt8]
    var index = 0

    mutating func space() {
        while index < bytes.count && [9, 10, 13, 32].contains(bytes[index]) { index += 1 }
    }

    mutating func string() throws -> String {
        let start = index
        try approvalRequire(index < bytes.count && bytes[index] == 34, "json", .invalidJSON)
        index += 1
        while index < bytes.count {
            let byte = bytes[index]
            index += 1
            if byte == 92 {
                try approvalRequire(index < bytes.count, "json", .invalidJSON)
                index += 1
            } else if byte == 34 {
                guard let value = try JSONSerialization.jsonObject(with: Data(bytes[start..<index]), options: .fragmentsAllowed) as? String else {
                    throw DeviceApprovalError(code: .invalidJSON, field: "json")
                }
                return value
            }
        }
        throw DeviceApprovalError(code: .invalidJSON, field: "json")
    }

    mutating func value(depth: Int = 0) throws {
        try approvalRequire(depth < 32, "json", .invalidJSON)
        space()
        try approvalRequire(index < bytes.count, "json", .invalidJSON)
        let first = bytes[index]
        if first == 34 { _ = try string(); return }
        if first == 123 || first == 91 {
            index += 1
            let close: UInt8 = first == 123 ? 125 : 93
            var keys = Set<String>()
            space()
            if index < bytes.count && bytes[index] == close { index += 1; return }
            while true {
                space()
                if first == 123 {
                    let key = try string()
                    try approvalRequire(keys.insert(key).inserted, key, .duplicateMember)
                    space()
                    try approvalRequire(index < bytes.count && bytes[index] == 58, "json", .invalidJSON)
                    index += 1
                }
                try value(depth: depth + 1)
                space()
                try approvalRequire(index < bytes.count, "json", .invalidJSON)
                if bytes[index] == close { index += 1; return }
                try approvalRequire(bytes[index] == 44, "json", .invalidJSON)
                index += 1
            }
        }
        let start = index
        while index < bytes.count && ![9, 10, 13, 32, 44, 93, 125].contains(bytes[index]) { index += 1 }
        try approvalRequire(index > start, "json", .invalidJSON)
    }
}

struct ApprovalJSONObject {
    let values: [String: Any]

    init(json: Data) throws {
        do {
            var scanner = ApprovalJSONScanner(bytes: Array(json))
            try scanner.value()
            scanner.space()
            try approvalRequire(scanner.index == scanner.bytes.count, "json", .invalidJSON)
            guard let object = try JSONSerialization.jsonObject(with: json) as? [String: Any] else {
                throw DeviceApprovalError(code: .invalidJSON, field: "json")
            }
            values = object
        } catch let error as DeviceApprovalError { throw error }
        catch { throw DeviceApprovalError(code: .invalidJSON, field: "json") }
    }

    init(values: [String: Any]) { self.values = values }

    func exact(_ names: [String], prefix: String = "") throws {
        let expected = Set(names)
        if let unknown = Set(values.keys).subtracting(expected).sorted().first {
            throw DeviceApprovalError(code: .unknownMember, field: prefix + unknown)
        }
        if let missing = expected.subtracting(values.keys).sorted().first {
            throw DeviceApprovalError(code: .missingMember, field: prefix + missing)
        }
    }

    func string(_ name: String, prefix: String = "") throws -> String {
        guard let value = values[name] as? String else {
            throw DeviceApprovalError(code: values[name] == nil ? .missingMember : .invalidField, field: prefix + name)
        }
        return value
    }
}
