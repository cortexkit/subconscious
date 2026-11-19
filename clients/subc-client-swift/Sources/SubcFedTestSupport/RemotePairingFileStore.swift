import CryptoKit
import Foundation
@testable import SubcFed

struct RemotePairingFileStore: FedRemotePairingStore {
    let directory: URL

    init(directory: URL) throws {
        let files = FileManager.default
        try files.createDirectory(at: directory, withIntermediateDirectories: true,
                                  attributes: [.posixPermissions: 0o700])
        let attributes = try files.attributesOfItem(atPath: directory.path)
        guard attributes[.type] as? FileAttributeType == .typeDirectory,
              let permissions = attributes[.posixPermissions] as? NSNumber,
              permissions.intValue == 0o700 else {
            throw FedRemotePairingError.storageUnavailable
        }
        self.directory = directory
    }

    func load(scope: String) throws -> Data? {
        do {
            return try Data(contentsOf: path(for: scope))
        } catch let error as CocoaError where error.code == .fileReadNoSuchFile {
            return nil
        }
    }

    func replace(scope: String, record: Data?) throws {
        let file = path(for: scope)
        if let record {
            try record.write(to: file, options: .atomic)
            try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
        } else {
            do {
                try FileManager.default.removeItem(at: file)
            } catch let error as CocoaError where error.code == .fileNoSuchFile {
                // Deletion is idempotent; there may be no pairing to recover.
            }
        }
    }

    private func path(for scope: String) -> URL {
        // Account IDs need not be safe file names.
        directory.appendingPathComponent(Data(SHA256.hash(data: Data(scope.utf8))).lowercaseHex)
    }
}
