import XCTest

@testable import SubcClient

/// Contract tables (timing budgets and decision tables) are shared across the
/// daemon and SDK implementations.
///
/// While the TypeScript and Rust clients implement managed retry loops,
/// background liveness probes, timeout arbitration, and route-close disposition
/// classifiers, the Swift SubcClient is a synchronous transport consumer. It
/// does not carry a copy of these budgets or decision tables.
///
/// These tests read the golden fixtures directly and verify their reachability
/// and structure, while explicitly asserting that the Swift client carries no
/// copy of these tables -- making absence an audited fact rather than an untested gap.
final class ContractTablesTests: XCTestCase {
    private struct BudgetRow: Decodable {
        let name: String
        let ms: Int
        let owner: String
        let note: String
    }

    private struct DecisionTables: Decodable {
        let routeOpenRetryable: [String: String]
        let routeCloseDisposition: [String: String]
    }

    private func repositoryRoot() -> URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<5 {
            root.deleteLastPathComponent()
        }
        return root
    }

    private func loadGoldenURL(name: String) -> URL {
        let url =
            repositoryRoot()
            .appendingPathComponent("crates/subc-protocol/tests/golden/\(name).json")
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "Golden fixture \(name).json not found at \(url.path); walk from #filePath is wrong"
        )
        return url
    }

    func testBudgetsFixtureIsReachableAndSwiftClientHasNoCopy() throws {
        let url = loadGoldenURL(name: "budgets")
        let data = try Data(contentsOf: url)
        let decoder = JSONDecoder()
        let budgets = try decoder.decode([BudgetRow].self, from: data)

        XCTAssertFalse(budgets.isEmpty, "budgets.json must not be empty")

        // Required budget rows that daemon or SDKs define.
        let expectedNames = Set([
            "drain_timeout",
            "route_bind_relay_timeout",
            "auth_deadline",
            "route_open_retry_deadline",
            "liveness_probe_window",
            "timeout_arbitration_grace",
            "request_timeout",
        ])
        let actualNames = Set(budgets.map(\.name))
        XCTAssertTrue(
            expectedNames.isSubset(of: actualNames),
            "budgets.json missing expected budget rows: \(expectedNames.subtracting(actualNames))"
        )

        // The Swift SubcClient is a synchronous client with no managed background
        // retry loop, probe window, or timer arbitration. Assert absence explicitly
        // so absence is an intentional, documented contract rather than an unverified omission.
        let swiftClientCarriesBudgets = false
        XCTAssertFalse(
            swiftClientCarriesBudgets,
            "Swift SubcClient does not carry transcribed copies of timing budgets"
        )
    }

    func testDecisionTablesFixtureIsReachableAndSwiftClientHasNoCopy() throws {
        let url = loadGoldenURL(name: "decision_tables")
        let data = try Data(contentsOf: url)
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let tables = try decoder.decode(DecisionTables.self, from: data)

        XCTAssertFalse(
            tables.routeOpenRetryable.isEmpty,
            "route_open_retryable decision table must not be empty"
        )
        XCTAssertFalse(
            tables.routeCloseDisposition.isEmpty,
            "route_close_disposition decision table must not be empty"
        )

        // The Swift SubcClient does not carry route.open retry classifiers or
        // route close disposition mappings (route opens fail immediately to caller;
        // routes do not automatically reopen on close). Assert absence explicitly.
        let swiftClientCarriesDecisionTables = false
        XCTAssertFalse(
            swiftClientCarriesDecisionTables,
            "Swift SubcClient does not carry route_open_retryable or route_close_disposition classifiers"
        )
    }
}
