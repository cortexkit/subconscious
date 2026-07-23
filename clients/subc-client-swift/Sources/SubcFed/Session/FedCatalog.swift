import Foundation

/// One management operation advertised by a remote module.
public struct FedCatalogOperation: Sendable, Equatable {
    public let name: String
    public let kind: String

    public init(name: String, kind: String) {
        self.name = name
        self.kind = kind
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

    /// Parses and filters a remote catalog. Duplicate module or operation names
    /// reject the whole snapshot. Unknown fields and invalid entries are dropped
    /// per fed-wire §7.2. Only mgmt-v1 operations are retained for the adapter.
    public static func parseRemote(
        frame: FedFrame,
        peerIncarnation: String,
        peerFeatures: Set<String>
    ) throws -> FedRemoteCatalog {
        guard frame.knownType == .catalog else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        guard case .integer(let generation) = frame.header["generation"] else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }

        let root: FedJSONObject
        do {
            root = try FedJSONObject(jsonData: frame.body)
        } catch {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        guard case .array(let moduleValues) = root["modules"] else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
        guard moduleValues.count <= 64 else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }

        var modules: [FedCatalogModule] = []
        var seenModuleIDs = Set<String>()

        for value in moduleValues {
            guard case .object(let moduleObject) = value else {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            guard case .string(let moduleID) = moduleObject["module_id"],
                  isValidIdentifier(moduleID)
            else {
                // Drop invalid module entries rather than rejecting the snapshot.
                continue
            }
            if seenModuleIDs.contains(moduleID) {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            seenModuleIDs.insert(moduleID)

            let version: String?
            if case .string(let value) = moduleObject["module_version"] {
                version = value
            } else {
                version = nil
            }

            var operations: [FedCatalogOperation] = []
            if peerFeatures.contains("mgmt-v1"),
               case .object(let management) = moduleObject["management"],
               case .array(let operationValues) = management["operations"]
            {
                guard operationValues.count <= 256 else {
                    throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
                }
                var seenNames = Set<String>()
                for operationValue in operationValues {
                    guard case .object(let op) = operationValue,
                          case .string(let name) = op["name"],
                          isValidOperationName(name),
                          case .string(let kind) = op["kind"],
                          kind == "query" || kind == "mutate"
                    else {
                        continue
                    }
                    if seenNames.contains(name) {
                        throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
                    }
                    seenNames.insert(name)
                    operations.append(FedCatalogOperation(name: name, kind: kind))
                }
            }

            // Tools are ignored by the management-only Swift origin adapter.
            // A module with neither tools nor operations is dropped.
            let hasTools: Bool
            if case .array(let tools) = moduleObject["tools"], !tools.isEmpty {
                hasTools = true
            } else {
                hasTools = false
            }
            if operations.isEmpty && !hasTools {
                continue
            }
            // Management-only modules with zero surviving operations drop entirely
            // when tools are also absent. When tools exist but we only care about
            // management, keep the module only if operations survived.
            if !operations.isEmpty {
                modules.append(FedCatalogModule(
                    moduleID: moduleID,
                    moduleVersion: version,
                    operations: operations
                ))
            }
        }

        return FedRemoteCatalog(
            generation: generation,
            modules: modules,
            peerIncarnation: peerIncarnation
        )
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
