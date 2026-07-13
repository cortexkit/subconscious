import Foundation
import XCTest
@testable import SubcClient

private enum TestTransportError: Error {
    case forcedReadFailure
    case insufficientBytes
}

private final class ScriptedTransport: Transport {
    private(set) var writes: [Data] = []
    private(set) var readSizes: [Int] = []
    private(set) var closeCount = 0
    var bytes: Data
    var failReadCalls: Set<Int> = []

    init(bytes: Data = Data()) {
        self.bytes = bytes
    }

    func append(_ data: Data) {
        bytes.append(data)
    }

    func writeAll(_ data: Data) throws {
        writes.append(data)
    }

    func readExact(_ n: Int) throws -> Data {
        readSizes.append(n)
        if failReadCalls.remove(readSizes.count) != nil {
            throw TestTransportError.forcedReadFailure
        }
        guard bytes.count >= n else { throw TestTransportError.insufficientBytes }
        let result = bytes.prefix(n)
        bytes.removeFirst(n)
        return Data(result)
    }

    func close() {
        closeCount += 1
    }
}

private func makeFrame(
    ty: FrameType,
    flags: UInt8 = 0,
    channel: UInt16,
    epoch: UInt32,
    corr: UInt64,
    json: Any? = nil
) throws -> Data {
    let body = try json.map { try JSONSerialization.data(withJSONObject: $0) } ?? Data()
    return encodeFrame(
        ty: ty,
        flags: flags,
        channel: channel,
        epoch: epoch,
        corr: corr,
        body: body
    )
}

private func decodeWrittenFrame(_ data: Data) throws -> Frame {
    let header = try decodeHeader(Data(data.prefix(HEADER_LEN)))
    return Frame(header: header, body: Data(data.dropFirst(HEADER_LEN)))
}

private func assertDecodeError(
    _ expected: DecodeError,
    bytes: Data,
    file: StaticString = #filePath,
    line: UInt = #line
) {
    XCTAssertThrowsError(try decodeHeader(bytes), file: file, line: line) { error in
        XCTAssertEqual(error as? DecodeError, expected, file: file, line: line)
    }
}

final class EnvelopeRevisionTests: XCTestCase {
    func testHeaderLayoutEpochBoundariesAndAdmissionClasses() throws {
        let cases: [(UInt32, AdmissionClass, FrameType)] = [
            (0, .normal, .request),
            (1, .expedite, .streamData),
            (UInt32.max, .sheddable, .push),
        ]

        for (epoch, admissionClass, type) in cases {
            let channel: UInt16 = epoch == 0 ? 0 : 42
            let header = EnvelopeHeader(
                len: 0,
                ver: PROTOCOL_VERSION,
                ty: type,
                flags: buildFlags(
                    binary: false,
                    priority: .interactive,
                    last: false,
                    admissionClass: admissionClass
                ),
                channel: channel,
                epoch: epoch,
                corr: UInt64.max
            )
            let encoded = encodeHeader(header)
            XCTAssertEqual(encoded.count, 21)
            let epochBytes = withUnsafeBytes(of: epoch.littleEndian, Array.init)
            XCTAssertEqual(Array(encoded[9..<13]), epochBytes)
            let decoded = try decodeHeader(encoded)
            XCTAssertEqual(decoded, header)
            XCTAssertEqual(decoded.admissionClass, admissionClass)
        }
    }

    func testDecodeRejectsEveryV2TaxonomyCaseExactly() throws {
        assertDecodeError(.tooShortForPrefix(have: 4), bytes: Data(repeating: 0, count: 4))

        var unsupported = Data(repeating: 0, count: FROZEN_PREFIX_LEN)
        unsupported[4] = 1
        assertDecodeError(.unsupportedVersion(ver: 1), bytes: unsupported)

        var shortHeader = Data(repeating: 0, count: 17)
        shortHeader[4] = PROTOCOL_VERSION
        assertDecodeError(.tooShortForHeader(have: 17, need: HEADER_LEN), bytes: shortHeader)

        let valid = EnvelopeHeader(
            len: 0,
            ver: PROTOCOL_VERSION,
            ty: .response,
            flags: 0,
            channel: 9,
            epoch: 1,
            corr: 1
        )

        var bytes = encodeHeader(valid)
        bytes[5] = 255
        assertDecodeError(.unknownFrameType(byte: 255), bytes: bytes)

        bytes = encodeHeader(valid)
        bytes[6] = 0b0100_0000
        assertDecodeError(.reservedFlagBits(flags: 0b0100_0000), bytes: bytes)

        bytes = encodeHeader(valid)
        bytes[6] = 0b0000_0110
        assertDecodeError(.reservedPriorityBits(flags: 0b0000_0110), bytes: bytes)

        bytes = encodeHeader(valid)
        bytes[6] = 0b0011_0000
        assertDecodeError(.reservedAdmissionClass(flags: 0b0011_0000), bytes: bytes)

        bytes = encodeHeader(valid)
        bytes[6] = 0b0010_0000
        assertDecodeError(
            .sheddableIllegalFrameType(ty: .response, flags: 0b0010_0000),
            bytes: bytes
        )

        var control = valid
        control.channel = 0
        control.epoch = 1
        assertDecodeError(.nonzeroEpochOnControlChannel(epoch: 1), bytes: encodeHeader(control))

        var goodbye = valid
        goodbye.ty = .goodbye
        goodbye.len = 1
        assertDecodeError(.pureHeaderFrameWithBody(ty: .goodbye, len: 1), bytes: encodeHeader(goodbye))
    }

    func testPrefixFirstReaderRejectsStalePureHeaderPromptly() throws {
        var staleHeader = Data(repeating: 0, count: 17)
        staleHeader[4] = 1
        staleHeader[5] = FrameType.goodbye.rawValue
        let transport = ScriptedTransport(bytes: staleHeader)

        XCTAssertThrowsError(try readFrame(from: transport)) { error in
            XCTAssertEqual(error as? DecodeError, .unsupportedVersion(ver: 1))
        }
        XCTAssertEqual(transport.readSizes, [FROZEN_PREFIX_LEN])
        XCTAssertEqual(transport.bytes.count, 12)
    }

    func testBodyCapIsCheckedAfterPrefixBeforeHeaderOrAllocation() throws {
        var prefix = Data()
        withUnsafeBytes(of: UInt32(MAX_FRAME_BODY_LEN + 1).littleEndian) {
            prefix.append(contentsOf: $0)
        }
        prefix.append(PROTOCOL_VERSION)
        let transport = ScriptedTransport(bytes: prefix)

        XCTAssertThrowsError(try readFrame(from: transport)) { error in
            XCTAssertEqual(
                error as? FrameReadError,
                .bodyTooLarge(len: UInt32(MAX_FRAME_BODY_LEN + 1), max: MAX_FRAME_BODY_LEN)
            )
        }
        XCTAssertEqual(transport.readSizes, [FROZEN_PREFIX_LEN])
    }
}

final class ClientWireRevisionTests: XCTestCase {
    func testStaleEpochIngressIsDroppedWithoutSettlingCurrentRequest() throws {
        let transport = ScriptedTransport()
        try transport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: 1,
            json: ["op": "route.open", "route_channel": 7, "route_epoch": 2]
        ))
        try transport.append(makeFrame(
            ty: .goodbye,
            channel: 7,
            epoch: 1,
            corr: 0
        ))
        try transport.append(makeFrame(
            ty: .response,
            channel: 7,
            epoch: 1,
            corr: 2,
            json: ["result": ["stale": true]]
        ))
        try transport.append(makeFrame(
            ty: .response,
            channel: 7,
            epoch: 2,
            corr: 2,
            json: ["result": ["current": true]]
        ))
        let client = SubcClient(transport: transport)
        let route = try client.routeOpenManagementSurface(
            moduleId: "module",
            projectRoot: "/tmp",
            harness: "test",
            session: "session"
        )

        let reply = try client.callManagement(route: route, method: "test")
        let result = reply["result"] as? [String: Bool]
        XCTAssertEqual(result?["current"], true)
        XCTAssertNil(result?["stale"])
        XCTAssertEqual(client.droppedIngressFrames, 2)
    }

    func testStaleHandleFromAnotherConnectionEmitsNoFrame() throws {
        let firstTransport = ScriptedTransport()
        try firstTransport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: 1,
            json: ["route_channel": 5, "route_epoch": 1]
        ))
        let firstClient = SubcClient(transport: firstTransport)
        let staleRoute = try firstClient.routeOpenManagementSurface(
            moduleId: "module",
            projectRoot: "/tmp",
            harness: "test",
            session: "session"
        )

        let secondTransport = ScriptedTransport()
        try secondTransport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: 1,
            json: ["route_channel": 5, "route_epoch": 1]
        ))
        let secondClient = SubcClient(transport: secondTransport)
        let currentRoute = try secondClient.routeOpenManagementSurface(
            moduleId: "module",
            projectRoot: "/tmp",
            harness: "test",
            session: "session"
        )
        XCTAssertEqual(currentRoute.channel, staleRoute.channel)
        XCTAssertEqual(currentRoute.epoch, staleRoute.epoch)
        XCTAssertTrue(currentRoute != staleRoute)
        let writesBeforeStaleOperations = secondTransport.writes.count

        XCTAssertThrowsError(
            try secondClient.callManagement(route: staleRoute, method: "must.not.send")
        ) { error in
            XCTAssertEqual(error as? ClientLocalError, .staleConnectionToken)
        }
        XCTAssertThrowsError(try secondClient.cancel(route: staleRoute, corr: 99)) { error in
            XCTAssertEqual(error as? ClientLocalError, .staleConnectionToken)
        }
        XCTAssertThrowsError(try secondClient.closeRoute(staleRoute)) { error in
            XCTAssertEqual(error as? ClientLocalError, .staleConnectionToken)
        }
        XCTAssertEqual(secondTransport.writes.count, writesBeforeStaleOperations)
    }

    func testLateSuccessfulRouteOpenIsClosedWithReturnedEpoch() throws {
        let transport = ScriptedTransport()
        transport.failReadCalls = [1]
        let client = SubcClient(transport: transport)

        XCTAssertThrowsError(try client.routeOpenManagementSurface(
            moduleId: "module",
            projectRoot: "/tmp",
            harness: "test",
            session: "session"
        ))

        try transport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: 1,
            json: ["route_channel": 12, "route_epoch": 44]
        ))
        try transport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: 2,
            json: ["modules": []]
        ))

        let modules = try client.catalogList()
        XCTAssertTrue(modules.isEmpty)
        XCTAssertEqual(transport.writes.count, 3)
        let goodbye = try decodeWrittenFrame(transport.writes[2])
        XCTAssertEqual(goodbye.header.ty, .goodbye)
        XCTAssertEqual(goodbye.header.channel, 12)
        XCTAssertEqual(goodbye.header.epoch, 44)
        XCTAssertEqual(goodbye.header.corr, 0)
    }

    func testAdmissionClassStampedAndIllegalRequestRejectedLocally() throws {
        let transport = ScriptedTransport()
        try transport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: 1,
            json: ["modules": []]
        ))
        let client = SubcClient(transport: transport)

        _ = try client.catalogList(admissionClass: .expedite)
        let expedited = try decodeWrittenFrame(transport.writes[0])
        XCTAssertEqual((expedited.header.flags >> 4) & 0b11, AdmissionClass.expedite.rawValue)

        XCTAssertThrowsError(try client.catalogList(admissionClass: .sheddable)) { error in
            XCTAssertEqual(
                error as? ClientLocalError,
                .illegalAdmissionClass(.sheddable, .request)
            )
        }
        XCTAssertEqual(transport.writes.count, 1)
    }

    func testCorrelationMaxEmittedOnceThenConnectionCloses() throws {
        let transport = ScriptedTransport()
        try transport.append(makeFrame(
            ty: .response,
            channel: 0,
            epoch: 0,
            corr: UInt64.max,
            json: ["modules": []]
        ))
        let client = SubcClient(transport: transport, nextCorr: UInt64.max)

        _ = try client.catalogList()
        let first = try decodeWrittenFrame(transport.writes[0])
        XCTAssertEqual(first.header.corr, UInt64.max)
        XCTAssertThrowsError(try client.catalogList()) { error in
            XCTAssertEqual(error as? ClientLocalError, .correlationExhausted)
        }
        XCTAssertEqual(transport.writes.count, 1)
        XCTAssertEqual(transport.closeCount, 1)
    }
}
