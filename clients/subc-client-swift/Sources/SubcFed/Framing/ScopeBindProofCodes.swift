/// Terminal scope admission codes, carried by `FedFailure.moduleError` without
/// extending its public enum. Only the holder of a scope's bind proof can bind it.
public struct ScopeBindProofCodes {
    public static let required = "scope_bind_proof_required"
    public static let mismatch = "scope_bind_proof_mismatch"

    static func message(for code: String) -> String? {
        switch code {
        case required: return "This scope requires its bind proof before a route can be opened."
        case mismatch: return "The bind proof does not match this scope."
        default: return nil
        }
    }
}
