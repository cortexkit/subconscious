import Foundation
@testable import SubcFed

/// Seeds remote-pairing receipts and scripts status connections for tests.
/// Use this from a test target only; never link it into an app product.
public struct RemotePairingTestSupport: Sendable {
    public enum StatusReply: Sendable {
        case granted
        case notGranted
        /// Close the authenticated status connection without a result. The
        /// public status call throws `.statusUnavailable` and keeps the receipt;
        /// `.checkStatus` is an outcome of the pairing ceremony, not status.
        case statusUnreachable
    }

    private let store: any FedRemotePairingStore

    /// Use the same Keychain receipt store as a normal `SubcFedClient`.
    public init() {
        store = FedKeychainRemotePairingStore()
    }

    /// Use a dedicated owner-only directory (created if needed) on unsigned
    /// SwiftPM hosts without access to the Data Protection Keychain. Signed
    /// app tests should use `init()` to exercise their real Keychain.
    public init(fileStoreDirectory: URL) throws {
        store = try RemotePairingFileStore(directory: fileStoreDirectory)
    }

    /// Replace the receipt for an isolated test account and device. Supply the
    /// account ID and local key used by the client's rendezvous identity.
    /// On a test host without Keychain access, the default store throws
    /// `.storageUnavailable` rather than falling back to a file.
    public func seedMatchedReceipt(for targetPublicKey: Data, accountID: String, devicePublicKey: Data) throws {
        let scope = try Self.scope(accountID: accountID, devicePublicKey: devicePublicKey, targetPublicKey: targetPublicKey)
        let entropy = SystemFedNoiseEntropy()
        let receipt = FedRemotePairReceipt(windowID: try entropy.randomBytes(count: 16), generation: 1,
            transcriptHash: try entropy.randomBytes(count: 32), targetKey: targetPublicKey, matched: true)
        try store.replace(scope: scope, record: JSONEncoder().encode(receipt))
    }

    /// Script the next authenticated status request for the seeded receipt. The
    /// signed rendezvous row for `target` must advertise an eligible direct
    /// candidate. The returned factory never opens a network socket.
    public func statusDialFactory(for target: FedNoiseKeyPair, accountID: String, devicePublicKey: Data,
                                  reply: StatusReply) throws -> FedNetworkDialFactory {
        let scope = try Self.scope(accountID: accountID, devicePublicKey: devicePublicKey, targetPublicKey: target.publicKey)
        let script = RemotePairingStatusScript(target: target, devicePublicKey: devicePublicKey, reply: reply)
        return FedNetworkDialFactory(connect: { [store] _, _ in
            guard let receipt = try FedRemotePairReceipt.load(store.load(scope: scope)),
                  receipt.targetKey == target.publicKey else { throw FedRemotePairingError.noReceipt }
            return try await script.open(request: receipt.request)
        }, relayUpgrade: { _ in throw FedRemotePairingError.statusUnavailable })
    }

    /// Create a fresh client that reads the same receipt store as this helper.
    /// For Keychain tests you can instead pass the factory to the public
    /// `SubcFedClient` initializer; it already uses the same store.
    public func makeClient(profile: FedPeerProfile, keyStore: any FedPrivateKeyStore, stateStore: any FedStateStore,
                           observedNetwork: @escaping SubcFedClient.ObservedNetworkProvider = { FedObservedNetworkSnapshot() },
                           pairingDialFactory: FedNetworkDialFactory = FedNetworkDialFactory()) -> SubcFedClient {
        SubcFedClient(profile: profile, keyStore: keyStore, stateStore: stateStore, observedNetwork: observedNetwork,
                      pairingDialFactory: pairingDialFactory, remotePairingStore: store)
    }

    private static func scope(accountID: String, devicePublicKey: Data, targetPublicKey: Data) throws -> String {
        guard !accountID.isEmpty, devicePublicKey.count == 32, targetPublicKey.count == 32,
              devicePublicKey != targetPublicKey else { throw FedRemotePairingError.invalidTarget }
        return "\(accountID):\(devicePublicKey.lowercaseHex)"
    }
}
