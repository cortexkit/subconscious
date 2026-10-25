import Foundation
import OSLog

/// One management operation advertised by a remote module.
public struct FedCatalogOperation: Sendable, Equatable {
    public let name: String
    public let kind: String
    public let delivery: Delivery?
    public let payload: FedOperationPayload?

    public struct Delivery: RawRepresentable, Sendable, Hashable, Codable {
        public let rawValue: String
        public init(rawValue: String) { self.rawValue = rawValue }
        public init(from decoder: Decoder) throws { self.init(rawValue: try decoder.singleValueContainer().decode(String.self)) }
        public func encode(to encoder: Encoder) throws {
            var container = encoder.singleValueContainer()
            try container.encode(rawValue)
        }
        public static let watch = Self(rawValue: "watch")
    }

    public init(name: String, kind: String, delivery: Delivery? = nil, payload: FedOperationPayload? = nil) {
        self.name = name
        self.kind = kind
        self.delivery = delivery
        self.payload = payload
    }

    public var isMutation: Bool {
        kind == "mutate" || kind == "mutating"
    }
}

/// One module entry after receiver-side filtering.
public struct FedCatalogModule: Sendable, Equatable {
    public let moduleID: String
    public let moduleVersion: String?
    public let operations: [FedCatalogOperation]

    public init(moduleID: String, moduleVersion: String? = nil, operations: [FedCatalogOperation]) {
        self.moduleID = moduleID
        self.moduleVersion = moduleVersion
        self.operations = operations
    }

    public func operation(named name: String) -> FedCatalogOperation? {
        operations.first { $0.name == name }
    }
}

/// Filtered remote catalog snapshot exposed to admission.
public struct FedRemoteCatalog: Sendable, Equatable {
    public let generation: UInt64
    public let modules: [FedCatalogModule]
    public let peerIncarnation: String

    public init(generation: UInt64, modules: [FedCatalogModule], peerIncarnation: String) {
        self.generation = generation
        self.modules = modules
        self.peerIncarnation = peerIncarnation
    }

    public func module(id: String) -> FedCatalogModule? {
        modules.first { $0.moduleID == id }
    }

    public func lookup(moduleID: String, operation: String) -> FedCatalogOperation? {
        module(id: moduleID)?.operation(named: operation)
    }
}

public enum FedCatalogCodec {
    private static let logger = Logger(subsystem: "io.cortexkit.subcfed", category: "session")
    /// Empty local catalog snapshot body. The Swift peer never advertises tools
    /// or management operations.
    public static let emptyBody = Data(#"{"modules":[]}"#.utf8)

    public static func emptySnapshotFrame(generation: UInt64) -> FedFrame {
        FedFrame(
            type: FedFrameType.catalog.rawValue,
            fields: ["generation": .integer(generation)],
            body: emptyBody
        )
    }

    /// Rejects unusable catalog envelopes, but isolates malformed or ambiguous
    /// entries so one module cannot disconnect the peer or hide healthy modules.
    /// All copies of duplicate module IDs and modules with duplicate operation
    /// names are dropped; only the first 64 modules with usable operations survive.
    /// Only entry fields used to build the returned remote catalog are validated.
    /// Unused entry members, including schemas, are ignored without numeric
    /// validation so newer catalog fields cannot hide modules from older receivers.
    /// Only negotiated mgmt-v1 operations are retained.
    public static func parseRemote(
        frame: FedFrame,
        peerIncarnation: String,
        peerFeatures: Set<String>
    ) throws -> FedRemoteCatalog {
        guard frame.knownType == .catalog else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        guard case .integer(let generation) = frame.header["generation"],
              generation < FedJSONValue.firstUnsafeInteger else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }

        let moduleValues: [FedParsedJSONValue]
        do {
            moduleValues = try parseRemoteBody(frame.body)
        } catch {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }

        var modules: [FedCatalogModule] = []
        var seenModuleIDs = Set<String>()
        var duplicateModuleIDs = Set<String>()
        // Discover ambiguity before filtering or capping, including duplicates
        // whose other copy would otherwise be malformed or beyond the cap.
        for value in moduleValues {
            guard case .object(let object) = value,
                  case .value(.string(let id)) = object["module_id"], isValidIdentifier(id)
            else { continue }
            if !seenModuleIDs.insert(id).inserted { duplicateModuleIDs.insert(id) }
        }

        moduleLoop: for value in moduleValues {
            guard case .object(let rawModule) = value else {
                logDrop(reason: "entry is not an object")
                continue
            }
            guard case .value(.string(let moduleID)) = rawModule["module_id"],
                  isValidIdentifier(moduleID)
            else {
                logDrop(reason: "invalid module identifier")
                continue
            }
            if duplicateModuleIDs.contains(moduleID) {
                logDrop(moduleID, reason: "duplicate module identifier")
                continue
            }

            if case .array(let tools) = rawModule["tools"] {
                guard tools.count <= 256 else {
                    logDrop(moduleID, reason: "more than 256 tools")
                    continue
                }
            }
            if case .object(let management) = rawModule["management"] {
                if case .array(let values) = management["operations"] {
                    guard values.count <= 256 else {
                        logDrop(moduleID, reason: "more than 256 operations")
                        continue
                    }
                    var names = Set<String>()
                    for value in values {
                        guard case .object(let op) = value,
                              case .value(.string(let name)) = op["name"], isValidOperationName(name),
                              case .value(.string(let kind)) = op["kind"], kind == "query" || kind == "mutate"
                        else { continue }
                        if !names.insert(name).inserted {
                            logDrop(moduleID, reason: "duplicate operation name")
                            continue moduleLoop
                        }
                    }
                }
            }

            let version: String?
            if case .value(.string(let value)) = rawModule["module_version"] {
                version = value
            } else {
                version = nil
            }

            var operations: [FedCatalogOperation] = []
            if peerFeatures.contains("mgmt-v1"),
               case .object(let management) = rawModule["management"],
               case .array(let operationValues) = management["operations"]
            {
                for operationValue in operationValues {
                    guard case .object(let op) = operationValue,
                          case .value(.string(let name)) = op["name"],
                          isValidOperationName(name),
                          case .value(.string(let kind)) = op["kind"],
                          kind == "query" || kind == "mutate"
                    else {
                        logDrop(moduleID, reason: "operation has invalid name or kind")
                        continue
                    }
                    var delivery: FedCatalogOperation.Delivery?
                    if let value = op["delivery"] {
                        guard case .value(.string(let text)) = value else {
                            logDrop(moduleID, reason: "operation has invalid delivery")
                            continue
                        }
                        delivery = .init(rawValue: text)
                    }
                    var payload: FedOperationPayload?
                    if peerFeatures.contains(FedFramedBodyCodec.feature), let declaration = op["payload"] {
                        guard delivery != .watch,
                              case .object(let object) = declaration,
                              case .value(.integer(let maximum)) = object["max_bytes"],
                              (1...UInt64(FedFramedBodyCodec.maximumDeclaredPayloadBytes)).contains(maximum)
                        else {
                            logDrop(moduleID, reason: "operation has invalid payload declaration")
                            continue
                        }
                        payload = FedOperationPayload(maxBytes: Int(maximum))
                    }
                    operations.append(FedCatalogOperation(name: name, kind: kind, delivery: delivery, payload: payload))
                }
            }

            guard !operations.isEmpty else {
                logDrop(moduleID, reason: "no usable management operations")
                continue
            }
            guard modules.count < 64 else {
                logDrop(moduleID, reason: "more than 64 usable modules")
                continue
            }
            modules.append(FedCatalogModule(
                moduleID: moduleID,
                moduleVersion: version,
                operations: operations
            ))
        }

        return FedRemoteCatalog(
            generation: generation,
            modules: modules,
            peerIncarnation: peerIncarnation
        )
    }

    // Framing and session decoding share the envelope check. Entry validation
    // happens only in parseRemote so framing cannot make module errors fatal.
    static func parseRemoteBody(_ body: Data) throws -> [FedParsedJSONValue] {
        guard case .object(let root) = try FedJSONValue.parse(body, allowUnrepresentableNumbers: true),
              case .array(let modules) = root["modules"]
        else { throw FedFrameError.invalidCatalogShape }
        for (key, value) in root where key != "modules" {
            guard !key.contains("\0") else { throw FedJSONError.invalidString }
            _ = try FedJSONObject(validating: [key: value.fedValue()])
        }
        return modules
    }

    private static func logDrop(_ moduleID: String? = nil, reason: String) {
        if let moduleID {
            logger.warning("Catalog drop module=\(moduleID, privacy: .public) reason=\(reason, privacy: .public)")
        } else {
            logger.warning("Catalog drop reason=\(reason, privacy: .public)")
        }
    }

    public static func isValidIdentifier(_ value: String) -> Bool {
        let utf8 = value.utf8
        guard (1...128).contains(utf8.count) else { return false }
        for byte in utf8 {
            if byte == UInt8(ascii: ":") || byte == UInt8(ascii: " ") || byte == UInt8(ascii: "\t") {
                return false
            }
            if byte < 0x20 || byte > 0x7E { return false }
        }
        return true
    }

    public static func isValidOperationName(_ value: String) -> Bool {
        let utf8 = value.utf8
        guard (1...128).contains(utf8.count) else { return false }
        for byte in utf8 {
            if byte == UInt8(ascii: ":") || byte == UInt8(ascii: " ") || byte == UInt8(ascii: "\t") {
                return false
            }
            // Dotted management names are permitted.
            if byte < 0x20 || byte > 0x7E { return false }
        }
        return true
    }
}

/// Applies generation monotonicity per peer incarnation.
public struct FedCatalogTracker: Sendable {
    public private(set) var applied: FedRemoteCatalog?
    public private(set) var lastGeneration: UInt64 = 0
    public private(set) var lastIncarnation: String?

    public init() {}

    public mutating func apply(_ catalog: FedRemoteCatalog) -> Bool {
        if let lastIncarnation, lastIncarnation == catalog.peerIncarnation {
            guard catalog.generation > lastGeneration else {
                return false
            }
        }
        // Unseen incarnation resets the baseline.
        applied = catalog
        lastGeneration = catalog.generation
        lastIncarnation = catalog.peerIncarnation
        return true
    }
}
