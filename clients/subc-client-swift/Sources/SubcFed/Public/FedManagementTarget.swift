import Foundation

/// Scopes management catalog lookups to one remote module. Changing modules
/// requires constructing or selecting another target; the library never splits
/// or rewrites method names across modules.
public struct FedManagementTarget: Sendable, Equatable, Hashable, Codable {
    public let moduleID: String

    public init(moduleID: String) throws {
        let trimmed = moduleID.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw FedFailure.invalidProfile(field: "moduleID")
        }
        self.moduleID = trimmed
    }
}
