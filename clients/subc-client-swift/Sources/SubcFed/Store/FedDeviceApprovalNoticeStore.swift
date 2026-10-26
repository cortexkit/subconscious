import Foundation

/// Optional store feature used only when the embedding app enables approvals.
/// The presentation callback runs before the seen mark is committed; a failed
/// commit may repeat a warning, but must never acknowledge an uncommitted mark.
public protocol FedDeviceApprovalNoticeStore: FedStateStore {
    func processDeviceApprovalNotice(
        _ notice: DeviceApprovalNotice, account: String, nowMS: UInt64,
        present: @Sendable (DeviceApprovalNotice) -> Void
    ) async throws
    func pruneDeviceApprovalNotices(nowMS: UInt64) async throws
}

struct FedDeviceApprovalSeenMark: Codable, Sendable {
    let account: String
    let noticeID: String
    let operationID: String
    let kind: DeviceApprovalNotice.Kind
    let version: UInt64
    let resolved: Bool
    let receivedAtMS: UInt64
    let deadlineMS: UInt64?

    static let retentionMS: UInt64 = 7 * 24 * 60 * 60 * 1_000

    func expired(at nowMS: UInt64) -> Bool {
        (nowMS >= receivedAtMS && nowMS - receivedAtMS >= Self.retentionMS)
            || (!resolved && deadlineMS.map { nowMS >= $0 } == true)
    }

    static func process(
        _ notice: DeviceApprovalNotice, account: String, nowMS: UInt64,
        marks: inout [Self], present: @Sendable (DeviceApprovalNotice) -> Void
    ) {
        marks.removeAll { $0.expired(at: nowMS) }
        let version = UInt64(notice.stateVersion)!
        let related = marks.filter { $0.account == account && $0.kind == notice.kind && $0.operationID == notice.operationID }
        let seen = marks.contains { $0.account == account && $0.noticeID == notice.noticeID && $0.version == version }
        let stale = related.contains { $0.version > version || ($0.resolved && !notice.isResolved) }
        let expired = !notice.isResolved && notice.deadlineMS.map { nowMS >= $0 } == true
        if !seen && !stale && !expired { present(notice) }
        if notice.isResolved && !stale {
            marks.removeAll { $0.account == account && $0.noticeID == notice.noticeID && !$0.resolved }
        }
        if !seen && !stale && !expired {
            marks.append(Self(account: account, noticeID: notice.noticeID, operationID: notice.operationID,
                              kind: notice.kind, version: version, resolved: notice.isResolved,
                              receivedAtMS: nowMS, deadlineMS: notice.deadlineMS))
        }
    }
}
