import Foundation

/// The held account merges that involve the caller's account, as listed by
/// `GET /v1/device/merges`. Each element is the same closed view that
/// `mergeStatus` returns, so the app can show pending merges without having
/// recorded their ids. The decoder keeps the server's order and accepts every
/// defined state; it doesn't filter or sort. An account without approval
/// enforcement gets an empty list.
public struct DeviceApprovalMergeList: Sendable {
    public let merges: [DeviceApprovalMergeView]
    /// Opaque continuation, or nil. The server sends null until it defines paging.
    public let cursor: String?
}

/// Why a merge-list reply could not be used. A non-200 status or a malformed body
/// always throws this error and never yields an empty list, so the app cannot
/// mistake a failed read for "no pending merges".
public struct DeviceApprovalMergeListError: Error, Equatable, Sendable {
    public let statusCode: Int
    /// The server's refusal when a non-200 body is one of the route's closed
    /// refusals; nil when the status was 200 or the refusal body was malformed.
    public let refusal: DeviceApprovalServerRefusal?
    /// The parsing failure for a malformed body; nil when `refusal` is set.
    public let reason: DeviceApprovalError?
}

/// The bodiless GET for the held-merge list. Send `method` and `path` with the
/// device bearer, then pass the raw reply bytes and HTTP status to `decodeReply`.
public struct DeviceApprovalMergeListRequest: Sendable {
    public var method: String { "GET" }
    public var body: Data? { nil }
    public var path: String { "/v1/device/merges" }

    /// Decode the closed reply `{"merges":[<MergeView>],"cursor":<string or null>}`.
    /// Only HTTP 200 succeeds. Unknown, missing and duplicate members refuse at
    /// every level; counters, versions and times must be decimal strings, never
    /// JSON numbers; ids must be canonical ULIDs; `state` and `automatic_hold`
    /// must be values the protocol defines.
    public func decodeReply(_ json: Data, statusCode: Int) throws -> DeviceApprovalMergeList {
        guard statusCode == 200 else {
            let refusal: DeviceApprovalServerRefusal
            do { refusal = try DeviceApprovalServerRefusal.decodeMergeList(json, statusCode: statusCode) }
            catch { throw Self.malformed(error, statusCode: statusCode) }
            throw DeviceApprovalMergeListError(statusCode: statusCode, refusal: refusal, reason: nil)
        }
        do {
            let object = try ApprovalJSONObject(json: json)
            try object.exact(["merges", "cursor"])
            guard let values = object.values["merges"] as? [[String: Any]] else {
                throw DeviceApprovalError(code: .invalidField, field: "merges")
            }
            let merges = try values.map { try DeviceApprovalMergeView(ApprovalJSONObject(values: $0)) }
            return DeviceApprovalMergeList(merges: merges, cursor: try object.cursor())
        } catch {
            throw Self.malformed(error, statusCode: statusCode)
        }
    }

    private static func malformed(_ error: Error, statusCode: Int) -> DeviceApprovalMergeListError {
        DeviceApprovalMergeListError(statusCode: statusCode, refusal: nil,
            reason: error as? DeviceApprovalError ?? DeviceApprovalError(code: .invalidJSON, field: "json"))
    }
}

public extension DeviceApprovalRequests {
    /// The held-merge list for the device's account. It is separate from
    /// `DeviceApprovalReadRequest` so no existing public enum gains a case.
    static func mergeList() -> DeviceApprovalMergeListRequest { DeviceApprovalMergeListRequest() }
}
