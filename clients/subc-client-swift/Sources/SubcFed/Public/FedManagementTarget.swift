import Foundation

/// Scopes management catalog lookups to one remote module. Changing modules
/// requires constructing or selecting another target; the library never splits
/// or rewrites method names across modules.
public struct FedManagementTarget: Sendable, Equatable, Hashable, Codable {
    public let moduleID: String

    private enum CodingKeys: String, CodingKey {
        case moduleID
    }

    public init(moduleID: String) throws {
        let trimmed = moduleID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw FedFailure.invalidProfile(field: "moduleID")
        }
        self.moduleID = trimmed
    }

    /// Validates on decode so a synthesized Codable path can never bypass the
    /// non-empty invariant the designated initializer enforces.
    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let raw = try container.decode(String.self, forKey: .moduleID)
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw FedFailure.invalidProfile(field: "moduleID")
        }
        self.moduleID = trimmed
    }
}
