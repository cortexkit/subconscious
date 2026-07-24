import CryptoKit
import Foundation
import XCTest
@testable import SubcFed

/// Contrastive regression tests for the Keychain first-load race. The production
/// store must return the persisted winner when two first-loaders race SecItemAdd,
/// never a divergent generated key.
final class FedKeychainStoreRaceTests: XCTestCase {
    /// Drives several concurrent first-loaders that all observe an empty store
    /// before any write lands, then asserts every caller sees the SAME key bytes
    /// and that those bytes equal what a fresh read returns (the persisted
    /// winner). Against the old generate-then-treat-duplicate-as-success logic the
    /// add-race loser returned its own generated key, so the loaders disagreed and
    /// this assertion failed.
    func testConcurrentFirstLoadAllObservePersistedWinner() async throws {
        let loaderCount = 4
        let backing = RacyFirstLoadBacking(firstReaders: loaderCount)
        let store = FedKeychainPrivateKeyStore(
            service: "race.test",
            noiseAccount: "noise",
            companionAccount: nil,
            backing: backing
        )

        // Fire loaderCount concurrent first loads; the backing holds their first
        // reads at a barrier so all of them observe absence before any add.
        let results = await withTaskGroup(of: Result<Data, Error>.self) { group in
            for _ in 0..<loaderCount {
                group.addTask {
                    do { return .success(try await store.staticPrivateKey()) }
                    catch { return .failure(error) }
                }
            }
            var collected: [Result<Data, Error>] = []
            for await result in group { collected.append(result) }
            return collected
        }

        let keys = try results.map { try $0.get() }
        XCTAssertEqual(keys.count, loaderCount)
        // All concurrent first-loaders must observe identical key bytes.
        let distinct = Set(keys)
        XCTAssertEqual(distinct.count, 1, "first-load race produced divergent static keys")

        // The agreed key must equal a subsequent fresh read (the persisted winner)
        // and the raw persisted item — the returned key is never a lost generated
        // key.
        let fresh = try await store.staticPrivateKey()
        XCTAssertEqual(fresh, keys[0])
        let persisted = await backing.persistedItem(account: "noise")
        XCTAssertEqual(persisted, keys[0])

        // The public key derives from that same persisted private key.
        let expectedPublic = try Curve25519.KeyAgreement.PrivateKey(rawRepresentation: keys[0])
            .publicKey.rawRepresentation
        let reportedPublic = try await store.staticPublicKey()
        XCTAssertEqual(reportedPublic, expectedPublic)
    }

    /// A single first load persists the generated key and every later read returns
    /// the same bytes (no flap across reads).
    func testSingleFirstLoadPersistsAndIsStableAcrossReads() async throws {
        let backing = RacyFirstLoadBacking(firstReaders: 1)
        let store = FedKeychainPrivateKeyStore(
            service: "race.test",
            noiseAccount: "noise",
            companionAccount: nil,
            backing: backing
        )
        let first = try await store.staticPrivateKey()
        let second = try await store.staticPrivateKey()
        XCTAssertEqual(first, second)
        let persisted = await backing.persistedItem(account: "noise")
        XCTAssertEqual(persisted, first)
    }
}

/// Controllable keychain backing that reproduces the SecItemAdd first-load race
/// deterministically: the first `firstReaders` reads are held at a barrier so all
/// concurrent first-loaders observe absence before any add lands. The first add
/// wins; later adds report a duplicate, exactly like errSecDuplicateItem.
private actor RacyFirstLoadBacking: FedKeychainBacking {
    private var persisted: [String: Data] = [:]
    private let barrier: FirstLoadBarrier
    private var firstReadsSeen = 0
    private let firstReaders: Int

    init(firstReaders: Int) {
        self.firstReaders = firstReaders
        self.barrier = FirstLoadBarrier(count: firstReaders)
    }

    func readItem(service: String, account: String) async throws -> Data? {
        // Only the initial empty-store reads rendezvous; once an item exists the
        // re-read of the winner and any later reads return it immediately.
        if persisted[account] == nil, firstReadsSeen < firstReaders {
            firstReadsSeen += 1
            await barrier.arriveAndWait()
        }
        return persisted[account]
    }

    func addItemIfAbsent(_ data: Data, service: String, account: String) async throws -> Bool {
        if persisted[account] != nil {
            return false
        }
        persisted[account] = data
        return true
    }

    func persistedItem(account: String) -> Data? {
        persisted[account]
    }
}

/// Countdown barrier: the first `count` callers block until the last one arrives,
/// then all are released together.
private actor FirstLoadBarrier {
    private let count: Int
    private var arrived = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(count: Int) { self.count = count }

    func arriveAndWait() async {
        arrived += 1
        if arrived >= count {
            let pending = waiters
            waiters.removeAll()
            for waiter in pending { waiter.resume() }
            return
        }
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            waiters.append(continuation)
        }
    }
}
