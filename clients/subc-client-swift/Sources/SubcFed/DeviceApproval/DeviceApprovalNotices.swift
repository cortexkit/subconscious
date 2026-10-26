import Foundation

/// Opt-in handling for verified control notices. Open the identity-bound store
/// before connecting, and keep the presentation sink alive for that connection.
public struct DeviceApprovalNoticeHandling: Sendable {
    public let store: any FedDeviceApprovalNoticeStore
    public let present: @Sendable (DeviceApprovalNotice) -> Void
    public let nowMS: @Sendable () -> UInt64

    public init(store: any FedDeviceApprovalNoticeStore,
                nowMS: @escaping @Sendable () -> UInt64 = { UInt64(Date().timeIntervalSince1970 * 1_000) },
                present: @escaping @Sendable (DeviceApprovalNotice) -> Void) {
        self.store = store; self.nowMS = nowMS; self.present = present
    }
}

/// The six signed rendezvous control notice types. Receipt is never consent to an action.
public struct DeviceApprovalNotice: Sendable {
    public enum Item: Sendable {
        case enrollment(DeviceApprovalEnrollmentView)
        case removal(DeviceApprovalRemovalView)
        case merge(DeviceApprovalMergeView)
        case resolved(kind: Kind, state: String)
    }

    public enum Kind: String, Sendable, Codable {
        case approval, removal, merge
        var idMember: String { self == .approval ? "enrollment_id" : rawValue + "_id" }
    }

    public let noticeID: String
    public let serverSeq: String
    public let operationID: String
    public let stateVersion: String
    public let kind: Kind
    public let item: Item

    public var isResolved: Bool {
        if case .resolved = item { return true }
        return false
    }

    /// Local pruning deadline for a pending card, not an authorization decision.
    public var deadlineMS: UInt64? {
        switch item {
        case .enrollment(let view): return UInt64(view.activateAfterMS.rawValue)
        case .removal(let view): return UInt64(view.executeAfterMS.rawValue)
        case .merge(let view): return UInt64(view.executeAfterMS.rawValue)
        case .resolved: return nil
        }
    }

    public static func isNoticeType(_ type: String) -> Bool {
        ["device_approval_pending", "device_approval_resolved", "device_removal_pending",
         "device_removal_resolved", "device_merge_pending", "device_merge_resolved"].contains(type)
    }

    /// Uses the same pinned account key and canonical signed envelope as registry frames.
    public static func verify(_ json: Data, pin: RdvAccountSigningKeyPin) throws -> Self {
        let envelope = try RdvSignedEnvelope.decode(RdvJSONValue.parseObject(json))
        try RdvSignedEnvelopeVerifier(pin: pin).verify(envelope)
        return try decode(envelope.payload)
    }

    public static func decode(_ payload: RdvJSONObject) throws -> Self {
        var fields = RdvFieldDecoder(payload)
        let type = try fields.string("type")
        guard isNoticeType(type), let kind = Kind(rawValue: String(type.split(separator: "_")[1])) else {
            throw DeviceApprovalError(code: .invalidField, field: "type")
        }
        let noticeID = try DeviceApprovalGrammar.ulid(fields.string("notice_id"), field: "notice_id").rawValue
        let seq = try fields.sequenceString("server_seq")
        let item: Item
        let operationID: String
        let version: String
        if type.hasSuffix("_resolved") {
            operationID = try DeviceApprovalGrammar.ulid(fields.string(kind.idMember), field: kind.idMember).rawValue
            version = try DeviceApprovalGrammar.decimal(fields.string("state_version"), field: "state_version", positive: true).rawValue
            let state = try fields.string("state")
            let states: [String]
            switch kind {
            case .approval: states = ["active", "cancelled"]
            case .removal: states = ["cancelled", "retiring", "removed", "rotated", "superseded"]
            case .merge: states = ["cancelled", "closing", "merged"]
            }
            try approvalRequire(states.contains(state), "state")
            item = .resolved(kind: kind, state: state)
        } else {
            let member = kind == .approval ? "enrollment" : kind.rawValue
            let bytes = try RdvCanonicalJSON.canonicalize(.object(fields.object(member)))
            switch kind {
            case .approval:
                let view = try DeviceApprovalEnrollmentView(json: bytes)
                try approvalRequire(view.state == "pending", "state")
                operationID = view.enrollmentID.rawValue
                version = view.stateVersion.rawValue
                item = .enrollment(view)
            case .removal:
                let view = try DeviceApprovalRemovalView(json: bytes)
                try approvalRequire(view.state == "pending", "state")
                operationID = view.removalID.rawValue
                version = view.stateVersion.rawValue
                item = .removal(view)
            case .merge:
                let view = try DeviceApprovalMergeView(json: bytes)
                try approvalRequire(view.state == "pending", "state")
                operationID = view.mergeID.rawValue
                version = view.stateVersion.rawValue
                item = .merge(view)
            }
        }
        try fields.finish()
        return Self(noticeID: noticeID, serverSeq: seq, operationID: operationID,
                    stateVersion: version, kind: kind, item: item)
    }
}

/// Receipt-only acknowledgement. There is no client outbox or server receipt;
/// each redelivery gets a fresh session sequence even when it was already seen.
public struct DeviceApprovalNoticeAck: Sendable, Equatable {
    public let kind: DeviceApprovalNotice.Kind
    public let seq: String
    public let noticeID: String
    public let operationID: String
    public let stateVersion: String

    public init(json: Data) throws {
        self = try Self.decode(RdvJSONValue.parseObject(json))
    }

    public init(notice: DeviceApprovalNotice, seq: String) throws {
        self = try Self.decode(RdvJSONObject([
            "type": .string("device_\(notice.kind.rawValue)_ack"), "seq": .string(seq),
            "notice_id": .string(notice.noticeID), notice.kind.idMember: .string(notice.operationID),
            "state_version": .string(notice.stateVersion),
        ]))
    }

    private init(kind: DeviceApprovalNotice.Kind, seq: String, noticeID: String, operationID: String, stateVersion: String) {
        self.kind = kind; self.seq = seq; self.noticeID = noticeID
        self.operationID = operationID; self.stateVersion = stateVersion
    }

    public static func decode(_ object: RdvJSONObject) throws -> Self {
        var fields = RdvFieldDecoder(object)
        let type = try fields.string("type")
        guard let kind = DeviceApprovalNotice.Kind.allNoticeKinds.first(where: { type == "device_\($0.rawValue)_ack" }) else {
            throw DeviceApprovalError(code: .invalidField, field: "type")
        }
        let seq = try fields.sequenceString("seq")
        let noticeID = try DeviceApprovalGrammar.ulid(fields.string("notice_id"), field: "notice_id").rawValue
        let operationID = try DeviceApprovalGrammar.ulid(fields.string(kind.idMember), field: kind.idMember).rawValue
        let version = try DeviceApprovalGrammar.decimal(fields.string("state_version"), field: "state_version", positive: true).rawValue
        try fields.finish()
        return Self(kind: kind, seq: seq, noticeID: noticeID, operationID: operationID, stateVersion: version)
    }

    public func encode() throws -> String {
        try RdvCanonicalJSON.canonicalString(.object(RdvJSONObject([
            "type": .string("device_\(kind.rawValue)_ack"), "seq": .string(seq),
            "notice_id": .string(noticeID), kind.idMember: .string(operationID),
            "state_version": .string(stateVersion),
        ])))
    }
}

private extension DeviceApprovalNotice.Kind {
    static let allNoticeKinds: [Self] = [.approval, .removal, .merge]
}

/// Untrusted APNs wake-up hint only. The app must fetch and verify the signed
/// notice before showing any action, and must never approve from push bytes.
public struct DeviceApprovalPush: Sendable, Equatable {
    public let kind: String
    public let noticeID: String

    public init(json: Data) throws {
        var fields = RdvFieldDecoder(try RdvJSONValue.parseObject(json))
        var aps = RdvFieldDecoder(try fields.object("aps"))
        try approvalRequire(try aps.string("alert") == "A security action is waiting for review", "aps.alert")
        try aps.finish()
        kind = try fields.string("kind")
        try approvalRequire(["device-approval-v1", "device-removal-v1", "device-merge-v1"].contains(kind), "kind")
        noticeID = try DeviceApprovalGrammar.ulid(fields.string("notice_id"), field: "notice_id").rawValue
        try fields.finish()
    }
}
