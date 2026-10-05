import Foundation
import XCTest
@testable import SubcFed

final class FedHelloMachineIDTests: XCTestCase {
    private let validID = "853ecdb6e1681c3dc8c1c578d14c23a2"

    func testMachineIDIsOptionalAndValidNamesReadBack() throws {
        let absent = try FedHelloCodec.parseRemoteHello(hello(machineID: nil))
        XCTAssertNil(absent.machineID)
        for id in [validID, String(repeating: "0", count: 32), String(repeating: "f", count: 32)] {
            let parsed = try FedHelloCodec.parseRemoteHello(hello(machineID: .string(id)))
            XCTAssertEqual(parsed.machineID, id)
        }
    }

    func testMalformedMachineIDsRefuseTheWholeHello() throws {
        let invalid: [FedJSONValue] = [
            .string(String(validID.dropLast())), .string(validID + "0"),
            .string(validID.uppercased()), .string("g" + String(validID.dropFirst())),
            .string(""), .integer(12), .boolean(false), .null,
            .array([]), .object(FedJSONObject([:])),
            .string(String(repeating: "é", count: 32)),
        ]
        for value in invalid {
            var gate = FedHelloGate()
            gate.noteLocalHelloSent()
            XCTAssertThrowsError(try gate.acceptRemote(
                frame: hello(machineID: value), localPolicy: try FedHelloPolicy(),
                localIncarnation: "00000000-0000-4000-8000-000000000001",
                localLedgerEpoch: "local", connectionAttemptID: nil, hasUnresolvedEffects: false
            )) {
                XCTAssertEqual($0 as? FedFailure, .protocolViolation(byeCode: "fed_limits_unsupported"))
            }
            XCTAssertFalse(gate.isComplete)
            XCTAssertFalse(gate.remoteHelloReceived)
            XCTAssertNil(gate.negotiation)
        }
    }

    func testMachineNameDoesNotRelaxOtherHelloValidation() throws {
        var fields = try hello(machineID: .string("bad")).header.dictionary
        fields["max_in_flight"] = .integer(0)
        XCTAssertThrowsError(try FedHelloCodec.parseRemoteHello(FedFrame(header: FedJSONObject(fields)))) {
            XCTAssertEqual($0 as? FedFailure, .protocolViolation(byeCode: "fed_limits_unsupported"))
        }
    }

    private func hello(machineID: FedJSONValue?) throws -> FedFrame {
        var fields = FedHelloCodec.buildLocalHello(
            policy: try FedHelloPolicy(),
            incarnation: "00000000-0000-4000-8000-0000000000aa",
            ledgerEpoch: "peer-epoch", connectionAttemptID: nil
        ).header.dictionary
        fields["machine_id"] = machineID
        return FedFrame(header: FedJSONObject(fields))
    }
}
