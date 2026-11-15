/// A successfully applied session catalog snapshot, numbered for one client.
public struct FedCatalogUpdate: Sendable, Equatable {
    /// Starts at 1 and increases for every applied snapshot, across reconnects.
    /// This is a client-local counter, not the peer's wire catalog generation.
    public let generation: UInt64

    public init(generation: UInt64) {
        self.generation = generation
    }
}
