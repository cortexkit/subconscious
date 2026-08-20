import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

final class FedPrivateKeyStoreProvenanceTests: XCTestCase {
    func testExistingNoiseKeyReportsPreexistingProvenance() async throws {
        let backing = TestKeychainBacking(existing: Data(repeating: 0x11, count: 32))
        let store = FedKeychainPrivateKeyStore(backing: backing)

        let beforeLoad = await store.noiseKeyProvenance()
        XCTAssertEqual(beforeLoad, .unknown)
        _ = try await store.staticPublicKey()
        let afterLoad = await store.noiseKeyProvenance()
        XCTAssertEqual(afterLoad, .preexisting)
    }

    func testMissingNoiseKeyReportsCreatedThisProcessProvenance() async throws {
        let backing = TestKeychainBacking()
        let store = FedKeychainPrivateKeyStore(backing: backing)

        _ = try await store.staticPrivateKey()
        let provenance = await store.noiseKeyProvenance()
        XCTAssertEqual(provenance, .createdThisProcess)
    }

    func testLostFirstAddRaceReportsCreatedThisProcessProvenance() async throws {
        let winner = Data(repeating: 0x22, count: 32)
        let backing = TestKeychainBacking(raceWinner: winner)
        let store = FedKeychainPrivateKeyStore(backing: backing)

        let loaded = try await store.staticPrivateKey()
        XCTAssertEqual(loaded, winner)
        let provenance = await store.noiseKeyProvenance()
        XCTAssertEqual(provenance, .createdThisProcess)
    }
}

private actor TestKeychainBacking: FedKeychainBacking {
    private var item: Data?
    private let raceWinner: Data?

    init(existing: Data? = nil, raceWinner: Data? = nil) {
        self.item = existing
        self.raceWinner = raceWinner
    }

    func readItem(service _: String, account _: String) async throws -> Data? {
        item
    }

    func addItemIfAbsent(_ data: Data, service _: String, account _: String) async throws -> Bool {
        if let raceWinner {
            item = raceWinner
            return false
        }
        guard item == nil else { return false }
        item = data
        return true
    }
}
