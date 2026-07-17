import Foundation
import XCTest
@testable import SubcChatAskSupport

// Pins the athena.get_consult per-attempt usage shape (camelCase wire).
// The absent-vs-zero distinction is contract: an ABSENT usage key means the
// provider reported nothing (render "unmeasured"), while a present all-zero
// object is a real measurement. A decoder that defaults missing usage to
// zeros would erase that distinction.
final class ObserveUsageDecodingTests: XCTestCase {
    private func decodeAttempt(_ json: String) throws -> ConsultAttempt {
        try JSONDecoder().decode(ConsultAttempt.self, from: Data(json.utf8))
    }

    func testAbsentUsageDecodesNil() throws {
        let a = try decodeAttempt(#"{"attemptId":"a1","phase":"fanout","state":"replied"}"#)
        XCTAssertNil(a.usage)
    }

    func testPresentZeroUsageIsMeasuredZero() throws {
        let a = try decodeAttempt(
            #"{"attemptId":"a2","usage":{"inputTokens":0,"cachedInputTokens":0,"cacheWriteTokens":0,"outputTokens":0,"reasoningTokens":0,"retriesUsed":0}}"#)
        let u = try XCTUnwrap(a.usage)
        XCTAssertEqual(u.inputTokens, 0)
        XCTAssertEqual(u.outputTokens, 0)
    }

    func testFullUsageDecodes() throws {
        let a = try decodeAttempt(
            #"{"attemptId":"a3","usage":{"inputTokens":1,"cachedInputTokens":52340,"cacheWriteTokens":8123,"outputTokens":691,"reasoningTokens":2048,"retriesUsed":1}}"#)
        let u = try XCTUnwrap(a.usage)
        // Anthropic cache-warmed shape: input=1, bulk under cached/cacheWrite.
        XCTAssertEqual(u.inputTokens, 1)
        XCTAssertEqual(u.cachedInputTokens, 52340)
        XCTAssertEqual(u.cacheWriteTokens, 8123)
        XCTAssertEqual(u.outputTokens, 691)
        XCTAssertEqual(u.reasoningTokens, 2048)
        XCTAssertEqual(u.retriesUsed, 1)
    }

    func testConsultDetailRollupDecodes() throws {
        let json = #"""
        {"consultId":"ct_x","tokenUsage":{"models":[
            {"model":"openai/gpt-5.6-sol","calls":2,"unmeasured":0,"retriesUsed":0,
             "input":1200,"cachedInput":30000,"cacheWrite":0,"output":900,"reasoning":400}],
          "total":{"calls":6,"unmeasured":2,"input":4000,"cachedInput":90000,
             "cacheWrite":8123,"output":2600,"reasoning":1100}}}
        """#
        let d = try JSONDecoder().decode(ConsultDetail.self, from: Data(json.utf8))
        let tu = try XCTUnwrap(d.tokenUsage)
        XCTAssertEqual(tu.models?.count, 1)
        XCTAssertEqual(tu.models?.first?.cachedInput, 30000)
        // The server total counts unmeasured attempts; client must not re-sum.
        XCTAssertEqual(tu.total?.unmeasured, 2)
    }

    func testTokenCountFormatting() {
        XCTAssertEqual(TokenFormat.count(999), "999")
        XCTAssertEqual(TokenFormat.count(1_500), "1.5k")
        XCTAssertEqual(TokenFormat.count(52_340), "52k")
        XCTAssertEqual(TokenFormat.count(2_400_000), "2.4M")
    }
}
