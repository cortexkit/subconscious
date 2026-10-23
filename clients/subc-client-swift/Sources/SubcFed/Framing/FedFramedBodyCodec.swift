import Foundation

/// Framing errors do not extend FedFailure, whose cases existing apps switch over.
public enum FedFramedBodyError: String, Error, Equatable, Sendable {
    case shortBody = "ShortBody"
    case emptyHead = "EmptyHead"
    case headTooLarge = "HeadTooLarge"
    case headLongerThanBody = "HeadLongerThanBody"
    case invalidJSON = "InvalidJson"
    case headNotObject = "HeadNotObject"
}

/// Local, pre-admission refusals for the framed management call overload.
public enum FedFramedCallError: Error, Equatable, Sendable {
    case featureNotNegotiated
    case payloadNotAccepted
    case payloadTooLarge(maxBytes: Int)
}

/// The largest raw payload one catalog operation accepts, taken from the
/// owning module's `operation_payload` declaration. The catalog carries it only
/// on sessions that negotiated `framed-calls-v1`.
public struct FedOperationPayload: Sendable, Equatable {
    public let maxBytes: Int
    public init(maxBytes: Int) { self.maxBytes = maxBytes }
}

/// Big-endian head length, strict JSON object head, then opaque payload bytes.
public enum FedFramedBodyCodec {
    public static let feature = "framed-calls-v1"
    public static let maximumHeadBytes = 65_536
    public static let maximumDeclaredPayloadBytes = 8_388_608

    public static func encode(head: FedJSONObject, payload: Data) throws -> Data {
        let json = try head.jsonData()
        guard json.count <= maximumHeadBytes else { throw FedFramedBodyError.headTooLarge }
        let length = UInt32(json.count)
        var body = Data([
            UInt8(truncatingIfNeeded: length >> 24), UInt8(truncatingIfNeeded: length >> 16),
            UInt8(truncatingIfNeeded: length >> 8), UInt8(truncatingIfNeeded: length)
        ])
        body.append(json)
        body.append(payload)
        return body
    }

    public static func decode(_ body: Data) throws -> (head: FedJSONObject, payload: Data) {
        guard body.count >= 4 else { throw FedFramedBodyError.shortBody }
        let length = body.prefix(4).reduce(0) { ($0 << 8) | Int($1) }
        guard length > 0 else { throw FedFramedBodyError.emptyHead }
        guard length <= maximumHeadBytes else { throw FedFramedBodyError.headTooLarge }
        guard length <= body.count - 4 else { throw FedFramedBodyError.headLongerThanBody }
        let head: FedJSONObject
        do {
            head = try FedJSONObject(jsonData: Data(body.dropFirst(4).prefix(length)))
        } catch FedJSONError.topLevelMustBeObject {
            throw FedFramedBodyError.headNotObject
        } catch {
            throw FedFramedBodyError.invalidJSON
        }
        return (head, Data(body.dropFirst(4 + length)))
    }
}
