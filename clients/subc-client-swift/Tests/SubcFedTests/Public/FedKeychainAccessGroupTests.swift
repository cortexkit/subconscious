import Foundation
import XCTest
@testable import SubcFed

final class FedKeychainAccessGroupTests: XCTestCase {
    func testConfiguredAccessGroupsReachNoiseAndCompanionOperations() async throws {
        let companion = Data(repeating: 0x33, count: 32)
        let backing = RecordingKeychainBacking(companion: companion)
        let store = FedKeychainPrivateKeyStore(
            noiseAccessGroup: "test.shared-noise",
            companionAccessGroup: "test.app-only-companion",
            backing: backing
        )

        _ = try await store.staticPrivateKey()
        let loadedCompanion = try await store.companionSigningPrivateKey()

        XCTAssertEqual(loadedCompanion, companion)
        let operations = await backing.recordedOperations()
        XCTAssertEqual(operations, [
            .read(account: "noise-static-x25519", accessGroup: "test.shared-noise"),
            .add(account: "noise-static-x25519", accessGroup: "test.shared-noise"),
            .read(account: "relay-companion-ed25519", accessGroup: "test.app-only-companion"),
        ])
    }

    func testNoiseDuplicateReReadUsesConfiguredAccessGroup() async throws {
        let winner = Data(repeating: 0x22, count: 32)
        let backing = RecordingKeychainBacking(raceWinner: winner)
        let store = FedKeychainPrivateKeyStore(
            noiseAccessGroup: "test.shared-noise",
            companionAccessGroup: "test.app-only-companion",
            backing: backing
        )

        let loaded = try await store.staticPrivateKey()

        XCTAssertEqual(loaded, winner)
        let operations = await backing.recordedOperations()
        XCTAssertEqual(operations, [
            .read(account: "noise-static-x25519", accessGroup: "test.shared-noise"),
            .add(account: "noise-static-x25519", accessGroup: "test.shared-noise"),
            .read(account: "noise-static-x25519", accessGroup: "test.shared-noise"),
        ])
    }

    func testDefaultAccessGroupsRemainUnspecified() async throws {
        let backing = RecordingKeychainBacking()
        let store = FedKeychainPrivateKeyStore(backing: backing)

        _ = try await store.staticPrivateKey()
        let loadedCompanion = try await store.companionSigningPrivateKey()

        XCTAssertNil(loadedCompanion)
        let operations = await backing.recordedOperations()
        XCTAssertEqual(operations, [
            .read(account: "noise-static-x25519", accessGroup: nil),
            .add(account: "noise-static-x25519", accessGroup: nil),
            .read(account: "relay-companion-ed25519", accessGroup: nil),
        ])
    }

    func testMissingCompanionInConfiguredGroupThrowsWithoutCreatingKey() async throws {
        let backing = RecordingKeychainBacking()
        let store = FedKeychainPrivateKeyStore(
            companionAccessGroup: "test.app-only-companion", backing: backing
        )

        do {
            _ = try await store.companionSigningPrivateKey()
            XCTFail("Missing companion in a configured group must not be treated as unenrolled")
        } catch {
            XCTAssertEqual(
                error as? FedCompanionKeyNotFoundError,
                FedCompanionKeyNotFoundError(accessGroup: "test.app-only-companion")
            )
        }

        let operations = await backing.recordedOperations()
        XCTAssertEqual(operations, [
            .read(account: "relay-companion-ed25519", accessGroup: "test.app-only-companion"),
        ])
    }

    func testDisabledCompanionAccountDoesNotReadConfiguredGroup() async throws {
        let backing = RecordingKeychainBacking()
        let store = FedKeychainPrivateKeyStore(
            companionAccount: nil,
            companionAccessGroup: "test.app-only-companion",
            backing: backing
        )

        let loadedCompanion = try await store.companionSigningPrivateKey()

        XCTAssertNil(loadedCompanion)
        let operations = await backing.recordedOperations()
        XCTAssertEqual(operations, [])
    }
}

private actor RecordingKeychainBacking: FedKeychainBacking {
    enum Operation: Equatable, Sendable {
        case read(account: String, accessGroup: String?)
        case add(account: String, accessGroup: String?)
    }

    private var operations: [Operation] = []
    private var noiseItem: Data?
    private let raceWinner: Data?
    private let companion: Data?

    init(raceWinner: Data? = nil, companion: Data? = nil) {
        self.raceWinner = raceWinner
        self.companion = companion
    }

    func readItem(service _: String, account: String, accessGroup: String?) async throws -> Data? {
        operations.append(.read(account: account, accessGroup: accessGroup))
        return account == "noise-static-x25519" ? noiseItem : companion
    }

    func addItemIfAbsent(
        _ data: Data, service _: String, account: String, accessGroup: String?
    ) async throws -> Bool {
        operations.append(.add(account: account, accessGroup: accessGroup))
        if let raceWinner {
            noiseItem = raceWinner
            return false
        }
        guard noiseItem == nil else { return false }
        noiseItem = data
        return true
    }

    func recordedOperations() -> [Operation] { operations }
}
