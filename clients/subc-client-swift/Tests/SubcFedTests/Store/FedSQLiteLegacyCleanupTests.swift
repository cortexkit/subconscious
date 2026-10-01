import Foundation
import XCTest
@testable import SubcFed

final class FedSQLiteLegacyCleanupTests: XCTestCase {
    private let localKey = Data(repeating: 0x11, count: 32)
    private let legacyNames = ["fed-state.json", "fed-state.json.migrated"]

    func testOpenDeletesBothLegacyFilesAndCreatesFreshStore() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        try writeLeftovers(in: dir)
        let unrelated = dir.appendingPathComponent("keep.json")
        try Data("keep".utf8).write(to: unrelated)

        let store = FedSQLiteStateStore(directoryURL: dir)
        let opened = try await store.open(localPublicKey: localKey)
        XCTAssertTrue(opened.created)
        XCTAssertTrue(opened.document.destinations.isEmpty)
        XCTAssertTrue(FileManager.default.fileExists(atPath: dir.appendingPathComponent(FedSQLiteStateStore.databaseFileName).path))
        for name in legacyNames {
            XCTAssertFalse(FileManager.default.fileExists(atPath: dir.appendingPathComponent(name).path), name)
        }
        XCTAssertEqual(try Data(contentsOf: unrelated), Data("keep".utf8))
    }

    func testExistingSQLiteDataSurvivesLegacyCleanup() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let first = FedSQLiteStateStore(directoryURL: dir)
        _ = try await first.open(localPublicKey: localKey)
        _ = try await first.reserveEffectSequence()
        let before = try await first.snapshot()
        try writeLeftovers(in: dir)

        let later = FedSQLiteStateStore(directoryURL: dir)
        let opened = try await later.open(localPublicKey: localKey)
        XCTAssertFalse(opened.created)
        XCTAssertEqual(opened.document, before)
        for name in legacyNames {
            XCTAssertFalse(FileManager.default.fileExists(atPath: dir.appendingPathComponent(name).path), name)
        }
    }

    func testFailedLegacyDeleteDoesNotFailOpen() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        try writeLeftovers(in: dir)
        let store = FedSQLiteStateStore(directoryURL: dir)
        await store.setLegacyFileRemover { _ in
            throw NSError(domain: NSPOSIXErrorDomain, code: Int(EACCES))
        }
        let opened = try await store.open(localPublicKey: localKey)
        XCTAssertTrue(opened.created)
        _ = try await store.reserveEffectSequence()
        for name in legacyNames {
            XCTAssertEqual(try Data(contentsOf: dir.appendingPathComponent(name)), Data("obsolete".utf8))
        }
    }

    func testCleanupDoesNotFollowSymlinksOrRemoveDirectoryContents() async throws {
        let dir = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let outside = try FedStoreUnderTest.temporaryDirectory(removedAfter: self)
        let target = outside.appendingPathComponent("saved.json")
        try Data("keep".utf8).write(to: target)
        let link = dir.appendingPathComponent(legacyNames[0])
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: target)
        let legacyDirectory = dir.appendingPathComponent(legacyNames[1])
        try FileManager.default.createDirectory(at: legacyDirectory, withIntermediateDirectories: false)
        let child = legacyDirectory.appendingPathComponent("keep")
        try Data("keep".utf8).write(to: child)

        let store = FedSQLiteStateStore(directoryURL: dir)
        _ = try await store.open(localPublicKey: localKey)
        XCTAssertFalse(FileManager.default.fileExists(atPath: link.path))
        XCTAssertEqual(try Data(contentsOf: target), Data("keep".utf8))
        XCTAssertEqual(try Data(contentsOf: child), Data("keep".utf8))
    }

    private func writeLeftovers(in dir: URL) throws {
        for name in legacyNames {
            try Data("obsolete".utf8).write(to: dir.appendingPathComponent(name))
        }
    }
}
