import Foundation

public enum FedWebSocketMessage: Sendable, Equatable {
    case binary(Data)
    case text(String)
}

public struct FedOuterRecordCodec: Sendable {
    public static let maximumNoiseMessageLength: UInt32 = 65_535
    public static let prefixLength = 4

    public init() {}

    public static func encode(_ noiseMessage: Data) throws -> Data {
        guard !noiseMessage.isEmpty else { throw FedCarrierError.emptyRecord }
        guard noiseMessage.count <= Int(maximumNoiseMessageLength) else {
            throw FedCarrierError.recordTooLarge(
                declared: UInt32(min(noiseMessage.count, Int(UInt32.max))),
                maximum: maximumNoiseMessageLength
            )
        }
        var output = Data(capacity: prefixLength + noiseMessage.count)
        var length = UInt32(noiseMessage.count).littleEndian
        withUnsafeBytes(of: &length) { output.append(contentsOf: $0) }
        output.append(noiseMessage)
        return output
    }

    public static func decodeTCPRecords(_ bytes: Data) throws -> [Data] {
        var decoder = FedTCPRecordDecoder()
        let records = try decoder.append(bytes)
        try decoder.finish()
        return records.map { Data($0) }
    }

    public static func decodeTCPRecord(_ record: Data) throws -> Data {
        guard record.count >= prefixLength else {
            throw FedCarrierError.incompleteRecord(expected: prefixLength, actual: record.count)
        }
        let declared = readLittleEndianUInt32(record, at: 0)
        guard declared > 0 else { throw FedCarrierError.emptyRecord }
        guard declared <= maximumNoiseMessageLength else {
            throw FedCarrierError.recordTooLarge(declared: declared, maximum: maximumNoiseMessageLength)
        }
        let expected = prefixLength + Int(declared)
        guard record.count == expected else {
            throw FedCarrierError.incompleteRecord(expected: expected, actual: record.count)
        }
        let start = record.startIndex + prefixLength
        return record.subdata(in: start..<(start + Int(declared)))
    }

    /// Decode a complete WebSocket binary message. Unlike TCP, no incomplete
    /// record is retained: the WebSocket message boundary is part of the wire
    /// contract and exactly one complete record must fill the message.
    public static func decodeWebSocketMessage(_ message: FedWebSocketMessage) throws -> Data {
        switch message {
        case .text:
            throw FedCarrierError.webSocketText
        case .binary(let bytes):
            return try decodeWebSocketBinaryMessage(bytes)
        }
    }

    public static func decodeWebSocketMessage(_ message: Data) throws -> Data {
        try decodeWebSocketBinaryMessage(message)
    }

    private static func decodeWebSocketBinaryMessage(_ message: Data) throws -> Data {
        guard message.count >= prefixLength else {
            if message.isEmpty { throw FedCarrierError.webSocketMessageEmpty }
            throw FedCarrierError.webSocketRecordSplit
        }
        let declared = readLittleEndianUInt32(message, at: 0)
        guard declared > 0 else { throw FedCarrierError.webSocketMessageEmpty }
        guard declared <= maximumNoiseMessageLength else {
            throw FedCarrierError.recordTooLarge(declared: declared, maximum: maximumNoiseMessageLength)
        }
        let actualPayload = message.count - prefixLength
        guard actualPayload == Int(declared) else {
            if actualPayload > Int(declared) {
                throw FedCarrierError.webSocketMultipleRecords
            }
            throw FedCarrierError.webSocketRecordMismatch(
                declared: declared,
                actualPayload: actualPayload
            )
        }
        let start = message.startIndex + prefixLength
        return message.subdata(in: start..<(start + actualPayload))
    }

    private static func readLittleEndianUInt32(_ data: Data, at offset: Int) -> UInt32 {
        let start = data.startIndex + offset
        return UInt32(data[start])
            | (UInt32(data[start + 1]) << 8)
            | (UInt32(data[start + 2]) << 16)
            | (UInt32(data[start + 3]) << 24)
    }
}

public struct FedTCPRecordDecoder: Sendable {
    private var buffer = Data()
    private var expectedPayloadLength: Int?

    public init() {}

    /// Appends one arbitrary TCP read and returns every complete outer record
    /// now available. The prefix is inspected before retaining a declared body.
    public mutating func append(_ bytes: Data) throws -> [Data] {
        guard !bytes.isEmpty else { return [] }
        if expectedPayloadLength == nil {
            var prefix = buffer
            let needed = max(0, FedOuterRecordCodec.prefixLength - prefix.count)
            if needed > 0 { prefix.append(contentsOf: bytes.prefix(needed)) }
            if prefix.count >= FedOuterRecordCodec.prefixLength {
                let declared = readLength(from: prefix)
                guard declared > 0 else { throw FedCarrierError.emptyRecord }
                guard declared <= FedOuterRecordCodec.maximumNoiseMessageLength else {
                    throw FedCarrierError.recordTooLarge(
                        declared: declared,
                        maximum: FedOuterRecordCodec.maximumNoiseMessageLength
                    )
                }
            }
        }
        buffer.append(bytes)
        var records: [Data] = []

        while true {
            if expectedPayloadLength == nil {
                guard buffer.count >= FedOuterRecordCodec.prefixLength else { return records }
                let declared = readLength(from: buffer)
                guard declared > 0 else { throw FedCarrierError.emptyRecord }
                guard declared <= FedOuterRecordCodec.maximumNoiseMessageLength else {
                    throw FedCarrierError.recordTooLarge(
                        declared: declared,
                        maximum: FedOuterRecordCodec.maximumNoiseMessageLength
                    )
                }
                expectedPayloadLength = Int(declared)
                buffer.removeFirst(FedOuterRecordCodec.prefixLength)
            }

            guard let expectedPayloadLength else { return records }
            guard buffer.count >= expectedPayloadLength else { return records }
            // Copy the payload so callers never receive a Data slice with a
            // retained, non-zero start index.
            records.append(Data(buffer.prefix(expectedPayloadLength)))
            buffer.removeFirst(expectedPayloadLength)
            self.expectedPayloadLength = nil
        }
    }

    public mutating func finish() throws {
        guard buffer.isEmpty, expectedPayloadLength == nil else {
            let expected = FedOuterRecordCodec.prefixLength + (expectedPayloadLength ?? buffer.count)
            throw FedCarrierError.incompleteRecord(expected: expected, actual: buffer.count)
        }
    }

    public var hasPartialRecord: Bool {
        expectedPayloadLength != nil || !buffer.isEmpty
    }

    private func readLength(from data: Data) -> UInt32 {
        let start = data.startIndex
        return UInt32(data[start])
            | (UInt32(data[start + 1]) << 8)
            | (UInt32(data[start + 2]) << 16)
            | (UInt32(data[start + 3]) << 24)
    }
}

public protocol FedTCPByteStream: Sendable {
    func write(_ bytes: Data) async throws
    func read() async throws -> Data?
    func close() async
}

public protocol FedWebSocketStream: Sendable {
    func send(_ message: FedWebSocketMessage) async throws
    func receive() async throws -> FedWebSocketMessage?
    func close() async
    /// Start transport-level liveness probing on a LONG-LIVED stream. A stream
    /// that goes quiet is declared dead so the pending `receive()` fails and the
    /// owner's read loop observes the death, instead of waiting forever on a
    /// half-open socket (server gone without a close frame: TCP ESTABLISHED,
    /// session dead, every mirror answer silently stale). Default is a no-op:
    /// short-lived streams (relay pipes have their own 4000-idle teardown) and
    /// test fakes do not probe.
    func startKeepalive(interval: TimeInterval, pongDeadline: TimeInterval) async
}

extension FedWebSocketStream {
    public func startKeepalive(interval: TimeInterval, pongDeadline: TimeInterval) async {}
}

public actor FedTCPRecordCarrier {
    public let kind = FedCarrierKind.tcp
    private let stream: any FedTCPByteStream
    private var decoder = FedTCPRecordDecoder()
    private var pendingRecords: [Data] = []
    private var readyToUse = true

    public init(stream: any FedTCPByteStream) {
        self.stream = stream
    }

    public func sendNoiseMessage(_ message: Data) async throws {
        guard readyToUse else { throw FedCarrierError.carrierClosed }
        do {
            try await stream.write(FedOuterRecordCodec.encode(message))
        } catch {
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func receiveNoiseMessage() async throws -> Data {
        guard readyToUse else { throw FedCarrierError.carrierClosed }
        if !pendingRecords.isEmpty { return pendingRecords.removeFirst() }
        do {
            while true {
                guard let bytes = try await stream.read() else {
                    readyToUse = false
                    try decoder.finish()
                    throw FedCarrierError.carrierClosed
                }
                pendingRecords.append(contentsOf: try decoder.append(bytes).map { Data($0) })
                if !pendingRecords.isEmpty { return pendingRecords.removeFirst() }
            }
        } catch {
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func close() async {
        guard readyToUse else { return }
        readyToUse = false
        await stream.close()
    }
}

public actor FedWebSocketRecordCarrier {
    public let kind = FedCarrierKind.webSocket
    private let stream: any FedWebSocketStream
    private var readyToUse = true

    public init(stream: any FedWebSocketStream) {
        self.stream = stream
    }

    public func sendNoiseMessage(_ message: Data) async throws {
        guard readyToUse else { throw FedCarrierError.carrierClosed }
        do {
            try await stream.send(.binary(try FedOuterRecordCodec.encode(message)))
        } catch {
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func receiveNoiseMessage() async throws -> Data {
        guard readyToUse else { throw FedCarrierError.carrierClosed }
        do {
            guard let message = try await stream.receive() else {
                readyToUse = false
                throw FedCarrierError.carrierClosed
            }
            return try FedOuterRecordCodec.decodeWebSocketMessage(message)
        } catch {
            readyToUse = false
            await stream.close()
            throw error
        }
    }

    public func close() async {
        guard readyToUse else { return }
        readyToUse = false
        await stream.close()
    }
}

public extension FedTCPRecordCarrier {
    static func establish(
        clock: any FedMonotonicClock,
        timeout: Duration = .seconds(3),
        connect: @escaping @Sendable () async throws -> any FedTCPByteStream
    ) async throws -> FedTCPRecordCarrier {
        let runner = FedStageDeadlineRunner(clock: clock)
        do {
            let stream = try await runner.run(
                stage: .carrierConnect,
                duration: timeout,
                operation: connect
            )
            return FedTCPRecordCarrier(stream: stream)
        } catch let error as FedDeadlineError {
            if case .timedOut(let stage) = error { throw FedCarrierError.timeout(stage) }
            throw error
        }
    }
}

public extension FedWebSocketRecordCarrier {
    static func establish(
        clock: any FedMonotonicClock,
        timeout: Duration = .seconds(3),
        upgrade: @escaping @Sendable () async throws -> any FedWebSocketStream
    ) async throws -> FedWebSocketRecordCarrier {
        let runner = FedStageDeadlineRunner(clock: clock)
        do {
            let stream = try await runner.run(
                stage: .webSocketUpgrade,
                duration: timeout,
                operation: upgrade
            )
            return FedWebSocketRecordCarrier(stream: stream)
        } catch let error as FedDeadlineError {
            if case .timedOut(let stage) = error { throw FedCarrierError.timeout(stage) }
            throw error
        }
    }
}
