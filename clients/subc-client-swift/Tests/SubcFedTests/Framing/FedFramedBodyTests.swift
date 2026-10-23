import Foundation
import XCTest
@testable import SubcFed

final class FedFramedBodyTests: XCTestCase {
    private var directory: URL {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return root.appendingPathComponent("vectors/framed-body-v1")
    }

    func testSharedBodyVectors() throws {
        let data = try Data(contentsOf: directory.appendingPathComponent("cases.json"))
        let cases = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let bodies = try XCTUnwrap(cases["bodies"] as? [[String: Any]])
        XCTAssertEqual(bodies.count, 10)
        for item in bodies {
            let name = try XCTUnwrap(item["file"] as? String)
            let body = try Data(contentsOf: directory.appendingPathComponent(name))
            if let error = item["error"] as? String {
                XCTAssertThrowsError(try FedFramedBodyCodec.decode(body), name) {
                    XCTAssertEqual(($0 as? FedFramedBodyError)?.rawValue, error, name)
                }
            } else {
                let decoded = try FedFramedBodyCodec.decode(body)
                let expected = try FedJSONObject(any: XCTUnwrap(item["head"]))
                XCTAssertEqual(decoded.head, expected, name)
                XCTAssertEqual(decoded.payload, try PairingVectors.hex(item["payload_hex"]), name)
                XCTAssertEqual(try FedFramedBodyCodec.encode(head: decoded.head, payload: decoded.payload), body, name)
            }
        }
    }

    func testSharedManifestPayloadDeclarationsProjectIntoCatalogOperations() throws {
        let data = try Data(contentsOf: directory.appendingPathComponent("cases.json"))
        let cases = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        let manifests = try XCTUnwrap(cases["manifests"] as? [[String: Any]])
        XCTAssertEqual(manifests.count, 6)
        for item in manifests {
            let name = try XCTUnwrap(item["file"] as? String)
            let manifest = try FedJSONObject(jsonData: Data(contentsOf: directory.appendingPathComponent(name)))
            guard case .object(let declarations) = manifest["operation_payload"],
                  case .array(let roles) = manifest["provides"],
                  case .object(let role) = roles.first,
                  case .array(let operations) = role["operations"]
            else { return XCTFail("bad manifest vector \(name)") }
            var projected: [FedJSONValue] = []
            for case .object(let operation) in operations {
                var fields: [String: FedJSONValue] = ["name": try XCTUnwrap(operation["name"]), "kind": try XCTUnwrap(operation["kind"])]
                if case .string(let method) = operation["name"] {
                    fields["payload"] = declarations[method]
                    if case .object(let delivery) = manifest["operation_delivery"] {
                        fields["delivery"] = delivery[method]
                    }
                }
                projected.append(.object(FedJSONObject(fields)))
            }
            let body: FedJSONObject = ["modules": .array([.object([
                "module_id": .string("upload"), "management": .object(["operations": .array(projected)])
            ])])]
            let frame = FedFrame(type: "catalog", fields: ["generation": .integer(1)], body: try body.jsonData())
            let catalog = try FedCatalogCodec.parseRemote(frame: frame, peerIncarnation: "peer",
                peerFeatures: ["mgmt-v1", FedFramedBodyCodec.feature])
            let op = catalog.lookup(moduleID: "upload", operation: "chunk")
            if item["error"] is NSNull {
                guard case .object(let declared) = declarations["chunk"], case .integer(let limit) = declared["max_bytes"]
                else { return XCTFail(name) }
                XCTAssertEqual(op?.payload?.maxBytes, Int(limit), name)
            } else {
                XCTAssertNil(op?.payload, name)
            }
        }
    }

    func testPayloadCatalogFieldRequiresNegotiationAndDoesNotAcceptManifestMapShape() throws {
        let frame = FedFrame(type: "catalog", fields: ["generation": .integer(1)], body: Data(#"{"modules":[{"module_id":"upload","operation_payload":{"mapped":{"max_bytes":2}},"management":{"operations":[{"name":"chunk","kind":"mutate","payload":{"max_bytes":8388608}},{"name":"mapped","kind":"mutate"}]}}]}"#.utf8))
        for features: Set<String> in [["mgmt-v1"], ["mgmt-v1", FedFramedBodyCodec.feature]] {
            let catalog = try FedCatalogCodec.parseRemote(frame: frame, peerIncarnation: "peer", peerFeatures: features)
            XCTAssertEqual(catalog.lookup(moduleID: "upload", operation: "chunk")?.payload?.maxBytes,
                features.contains(FedFramedBodyCodec.feature) ? 8_388_608 : nil)
            XCTAssertNil(catalog.lookup(moduleID: "upload", operation: "mapped")?.payload)
        }
    }

    func testFramedCallHeaderRequiresNegotiation() throws {
        let body = try Data(contentsOf: directory.appendingPathComponent("small.bin"))
        let header = Data(try Data(contentsOf: directory.appendingPathComponent("fed-call-header.json")).dropLast())
        XCTAssertThrowsError(try FedFrameCodec.encode(headerData: header, body: body, negotiationComplete: true, negotiatedFeatures: ["mgmt-v1"]))
        let features: Set<String> = ["mgmt-v1", FedFramedBodyCodec.feature]
        let wire = try FedFrameCodec.encode(headerData: header, body: body, negotiationComplete: true, negotiatedFeatures: features)
        XCTAssertEqual(wire, try Data(contentsOf: directory.appendingPathComponent("fed-call.bin")))
        let decoded = try FedFrameCodec.decode(wire, negotiationComplete: true, negotiatedFeatures: features)
        XCTAssertEqual(decoded.header["framed"], .boolean(true))
        XCTAssertNil(decoded.header["binary"])
        XCTAssertEqual(decoded.body, body)
    }
}
