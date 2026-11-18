import Foundation
import Security

/// SubcFed owns the receipt, so app code and its dependencies cannot forge a
/// local match through the SDK API. This does not defend against hostile code
/// inside the embedding process, which shares the app's Keychain access.
/// A write must durably replace or delete the record for that account/device.
protocol FedRemotePairingStore: Sendable {
    func load(scope: String) throws -> Data?
    func replace(scope: String, record: Data?) throws
}

/// Default remote pairing storage. Records do not migrate to another device.
struct FedKeychainRemotePairingStore: FedRemotePairingStore {
    private func query(_ scope: String) -> [String: Any] {
        [kSecClass as String: kSecClassGenericPassword,
         kSecAttrService as String: "io.cortexkit.subcfed.remote-pairing.v1",
         kSecAttrAccount as String: scope,
         kSecUseDataProtectionKeychain as String: true]
    }

    func load(scope: String) throws -> Data? {
        var query = query(scope)
        query[kSecReturnData as String] = true
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let value = result as? Data else { throw FedRemotePairingError.storageUnavailable }
        return value
    }

    func replace(scope: String, record: Data?) throws {
        let query = query(scope)
        guard let record else {
            let status = SecItemDelete(query as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else {
                throw FedRemotePairingError.storageUnavailable
            }
            return
        }
        let update = [kSecValueData as String: record]
        var status = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var item = query
            item[kSecValueData as String] = record
            item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            status = SecItemAdd(item as CFDictionary, nil)
        }
        guard status == errSecSuccess else { throw FedRemotePairingError.storageUnavailable }
    }
}

struct FedRemotePairReceipt: Codable, Equatable, Sendable {
    let windowID: Data
    let generation: UInt64
    let transcriptHash: Data
    let targetKey: Data
    let matched: Bool

    static let consumed = Data([0])

    static func load(_ record: Data?) throws -> Self? {
        guard let record, record != consumed else { return nil }
        guard let receipt = try? JSONDecoder().decode(Self.self, from: record), receipt.matched,
              receipt.windowID.count == 16, receipt.generation > 0,
              receipt.transcriptHash.count == 32, receipt.targetKey.count == 32 else {
            throw FedRemotePairingError.storageUnavailable
        }
        return receipt
    }

    var request: FedFrame {
        FedFrame(type: FedRemotePairCodec.status, fields: [
            "window_id": .string(windowID.lowercaseHex), "generation": .string(String(generation)),
            "transcript_hash": .string(transcriptHash.lowercaseHex),
        ])
    }
}
