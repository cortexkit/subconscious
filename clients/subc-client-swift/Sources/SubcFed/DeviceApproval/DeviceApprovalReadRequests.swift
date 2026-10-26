import Foundation

public enum DeviceApprovalReadRoute: String, Sendable {
    case enrollmentStatus, approvals, removals, mergeStatus
}

public enum DeviceApprovalReadReply: Sendable {
    case enrollment(DeviceApprovalEnrollmentView, retryAfterSeconds: UInt64)
    case approvals([DeviceApprovalEnrollmentView], cursor: String?)
    case removals([DeviceApprovalRemovalView], cursor: String?)
    case merge(DeviceApprovalMergeView)
    case refused(DeviceApprovalServerRefusal)
}

extension ApprovalJSONObject {
    func cursor(_ name: String = "cursor") throws -> String? {
        if values[name] is NSNull { return nil }
        return try string(name)
    }
}

public struct DeviceApprovalReadRequest: Sendable {
    public var method: String { "GET" }
    public var body: Data? { nil }
    public let path: String
    public let route: DeviceApprovalReadRoute
    let expectedID: DeviceApprovalValue?

    public func decodeReply(_ json: Data, statusCode: Int) throws -> DeviceApprovalReadReply {
        if statusCode != 200 {
            return .refused(try DeviceApprovalServerRefusal.decodeRead(json, statusCode: statusCode, route: route))
        }
        let object = try ApprovalJSONObject(json: json)
        switch route {
        case .enrollmentStatus:
            try object.exact(["enrollment", "retry_after_s"])
            let view = try DeviceApprovalEnrollmentView(object.object("enrollment"))
            try approvalRequire(view.enrollmentID == expectedID, "enrollment_id")
            return .enrollment(view, retryAfterSeconds: try object.integer("retry_after_s"))
        case .approvals:
            try object.exact(["enrollments", "cursor"])
            guard let values = object.values["enrollments"] as? [[String: Any]] else {
                throw DeviceApprovalError(code: .invalidField, field: "enrollments")
            }
            return .approvals(try values.map { try DeviceApprovalEnrollmentView(ApprovalJSONObject(values: $0)) }, cursor: try object.cursor())
        case .removals:
            try object.exact(["removals", "cursor"])
            guard let values = object.values["removals"] as? [[String: Any]] else {
                throw DeviceApprovalError(code: .invalidField, field: "removals")
            }
            return .removals(try values.map { try DeviceApprovalRemovalView(ApprovalJSONObject(values: $0)) }, cursor: try object.cursor())
        case .mergeStatus:
            try object.exact(["merge"])
            let view = try DeviceApprovalMergeView(object.object("merge"))
            try approvalRequire(view.mergeID == expectedID, "merge_id")
            return .merge(view)
        }
    }
}

public extension DeviceApprovalRequests {
    static func enrollmentStatus(enrollmentID: String) throws -> DeviceApprovalReadRequest {
        let id = try DeviceApprovalGrammar.ulid(enrollmentID, field: "enrollment_id")
        return DeviceApprovalReadRequest(path: "/v1/device/enrollments/\(id.rawValue)", route: .enrollmentStatus, expectedID: id)
    }

    static func approvals() -> DeviceApprovalReadRequest {
        DeviceApprovalReadRequest(path: "/v1/device/approvals", route: .approvals, expectedID: nil)
    }

    static func removals(cursor: String? = nil) -> DeviceApprovalReadRequest {
        var path = "/v1/device/removals"
        if let cursor {
            // A cursor is opaque; encode its exact UTF-8 bytes as one query value.
            let encoded = cursor.utf8.map { byte -> String in
                if (65...90).contains(byte) || (97...122).contains(byte) || (48...57).contains(byte) || [45, 46, 95, 126].contains(byte) {
                    return String(UnicodeScalar(byte))
                }
                return String(format: "%%%02X", byte)
            }.joined()
            path += "?cursor=" + encoded
        }
        return DeviceApprovalReadRequest(path: path, route: .removals, expectedID: nil)
    }

    static func mergeStatus(mergeID: String) throws -> DeviceApprovalReadRequest {
        let id = try DeviceApprovalGrammar.ulid(mergeID, field: "merge_id")
        return DeviceApprovalReadRequest(path: "/v1/device/merges/\(id.rawValue)", route: .mergeStatus, expectedID: id)
    }
}
