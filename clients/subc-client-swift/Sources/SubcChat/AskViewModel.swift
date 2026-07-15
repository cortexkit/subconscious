import Foundation
import SwiftUI
import SubcClient
import SubcChatAskSupport

/// Drives the pending user-ask management surface. The list polls while visible;
/// selecting an ask loads its current record, and every action reads it again so the
/// server remains the source of truth for answers, cancellations, and deadlines.
@MainActor
final class AskViewModel: ObservableObject {
    @Published var asks: [AskRequest] = []
    @Published var askDetail: AskRequest?
    @Published var selectedAskId: String?
    @Published var status: String = "idle"
    @Published var opsAvailable: Bool? = nil
    @Published var actionNotice: String?
    @Published var isSubmitting = false

    private let work = DispatchQueue(label: "subc-asks.client", qos: .userInitiated)
    private let worker: AskManagementWorker
    private var completedRequestIDs = Set<String>()

    init() {
        // Reuse the rooms identity so every app management tab is one ck-app caller.
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("CortexKitChat", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let idFile = dir.appendingPathComponent("rooms-identity.txt")
        let sessionId: String
        if let existing = try? String(contentsOf: idFile, encoding: .utf8),
           !existing.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            sessionId = existing.trimmingCharacters(in: .whitespacesAndNewlines)
        } else {
            let minted = "ckapp-\(UUID().uuidString)"
            try? minted.write(to: idFile, atomically: true, encoding: .utf8)
            sessionId = minted
        }
        worker = AskManagementWorker(
            connectionFile: NSString(string: "~/.local/share/cortexkit/run/subc-connection.json").expandingTildeInPath,
            harness: "ck-app",
            sessionId: sessionId,
            callerDirectory: dir.path)
    }

    var tabTitle: String {
        asks.isEmpty ? "Asks" : "Asks (\(asks.count))"
    }

    var selectedListAsk: AskRequest? {
        guard let selectedAskId else { return nil }
        return asks.first { $0.requestID == selectedAskId }
    }

    var hasTransientError: Bool {
        status.contains("failed")
    }

    // MARK: Lifecycle

    func appear() {
        pollPending()
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: 5, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.pollPending() }
        }
    }

    func disappear() {
        timer?.invalidate(); timer = nil
    }

    private var timer: Timer?

    // MARK: Polling and selection

    private func pollPending() {
        let worker = worker
        work.async { [weak self, worker] in
            do {
                // The server specifies ascending askedAt order; preserve it rather
                // than sorting client-side so ties retain the server's order too.
                let asks = try worker.pendingAsksBlocking()
                DispatchQueue.main.async {
                    self?.asks = asks
                    self?.opsAvailable = true
                    self?.status = "live"
                }
            } catch {
                let message = shortError(error)
                let lowercased = message.lowercased()
                let opsUnavailable = lowercased.contains("unknown")
                    || lowercased.contains("unsupported")
                    || lowercased.contains("no such")
                if !opsUnavailable { worker.close() }
                DispatchQueue.main.async {
                    guard let self else { return }
                    if opsUnavailable {
                        self.opsAvailable = false
                        self.status = "waiting for alfonso-core ops"
                    } else {
                        self.status = "poll failed: \(message)"
                    }
                }
            }
        }
    }

    func selectAsk(_ id: String) {
        guard selectedAskId != id else { return }
        selectedAskId = id
        askDetail = nil
        actionNotice = nil
        loadAsk(id)
    }

    private func loadAsk(_ requestID: String) {
        let worker = worker
        work.async { [weak self, worker] in
            do {
                let ask = try worker.askBlocking(requestID: requestID)
                DispatchQueue.main.async {
                    guard let self, self.selectedAskId == requestID else { return }
                    if let ask {
                        self.askDetail = ask
                    } else {
                        self.asks.removeAll { $0.requestID == requestID }
                        self.selectedAskId = nil
                        self.actionNotice = "Ask no longer exists"
                    }
                }
            } catch {
                let message = shortError(error)
                worker.close()
                DispatchQueue.main.async {
                    self?.status = "detail failed: \(message)"
                }
            }
        }
    }

    // MARK: Actions

    func canAct(on ask: AskRequest) -> Bool {
        !isSubmitting && ask.isPending && !completedRequestIDs.contains(ask.requestID)
    }

    func persistAnswer(_ answer: String) {
        let trimmed = answer.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            status = "answer failed: an answer is required"
            return
        }
        guard let ask = selectedActionAsk(), canAct(on: ask) else { return }

        isSubmitting = true
        actionNotice = nil
        let requestID = ask.requestID
        let worker = worker
        work.async { [weak self, worker] in
            do {
                let raw = try worker.alfonsoCallBlocking(
                    "ask.persist_answer",
                    ["requestID": requestID, "answer": answer])
                let outcome = try AskPersistAnswerReplyParser.parse(raw)
                let refresh = worker.refreshAfterMutationBlocking(requestID: requestID)
                DispatchQueue.main.async {
                    self?.applyAnswerOutcome(outcome, requestID: requestID, refresh: refresh)
                }
            } catch {
                let message = shortError(error)
                worker.close()
                DispatchQueue.main.async {
                    self?.finishActionFailure("answer", message)
                }
            }
        }
    }

    func dismiss(resolution: String?) {
        guard let ask = selectedActionAsk(), canAct(on: ask) else { return }
        guard let askerSessionID = ask.askerSessionID, !askerSessionID.isEmpty else {
            status = "dismiss failed: ask has no asker session"
            return
        }

        isSubmitting = true
        actionNotice = nil
        let requestID = ask.requestID
        var params: [String: Any] = [
            "requestID": requestID,
            "askerSessionID": askerSessionID,
        ]
        if let resolution, !resolution.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            params["resolution"] = resolution
        }
        let worker = worker
        work.async { [weak self, worker] in
            do {
                let raw = try worker.alfonsoCallBlocking("ask.resolve_user_ask", params)
                try validateAskDismissReply(raw)
                let refresh = worker.refreshAfterMutationBlocking(requestID: requestID)
                DispatchQueue.main.async {
                    self?.applyDismissal(requestID: requestID, refresh: refresh)
                }
            } catch {
                let message = shortError(error)
                worker.close()
                DispatchQueue.main.async {
                    self?.finishActionFailure("dismiss", message)
                }
            }
        }
    }

    private func selectedActionAsk() -> AskRequest? {
        if let detail = askDetail, detail.requestID == selectedAskId { return detail }
        return selectedListAsk
    }

    private func applyAnswerOutcome(
        _ outcome: AskPersistAnswerOutcome,
        requestID: String,
        refresh: MutationRefresh
    ) {
        isSubmitting = false
        completedRequestIDs.insert(requestID)
        actionNotice = outcome.presentation
        if let asks = refresh.asks {
            self.asks = asks
        }

        if selectedAskId == requestID {
            if let detail = refresh.detail {
                askDetail = detail
            } else if let returned = outcome.request {
                // Conflict and cancellation replies carry the authoritative record
                // even when the subsequent read has already been pruned.
                askDetail = returned
            } else {
                askDetail = nil
                selectedAskId = nil
            }
        }

        if case .notFound = outcome {
            asks.removeAll { $0.requestID == requestID }
            if selectedAskId == requestID {
                askDetail = nil
                selectedAskId = nil
            }
        }
        finishMutationRefresh(refresh)
    }

    private func applyDismissal(requestID: String, refresh: MutationRefresh) {
        isSubmitting = false
        completedRequestIDs.insert(requestID)
        actionNotice = "Ask dismissed."
        if let asks = refresh.asks {
            self.asks = asks
        } else {
            asks.removeAll { $0.requestID == requestID }
        }
        if selectedAskId == requestID {
            askDetail = refresh.detail
            if refresh.detail == nil { selectedAskId = nil }
        }
        finishMutationRefresh(refresh)
    }

    private func finishMutationRefresh(_ refresh: MutationRefresh) {
        if let errorMessage = refresh.errorMessage {
            status = errorMessage
        } else {
            opsAvailable = true
            status = "live"
        }
    }

    private func finishActionFailure(_ label: String, _ message: String) {
        isSubmitting = false
        status = "\(label) failed: \(message)"
    }
}

/// Serializes alfonso-core operations onto AskViewModel's private work queue. This
/// isolates the blocking client and route handles from SwiftUI's main-actor state.
private final class AskManagementWorker: @unchecked Sendable {
    private let connectionFile: String
    private let harness: String
    private let sessionId: String
    private let callerDirectory: String
    private var client: SubcClient?
    private var alfonsoRoute: RouteHandle?

    init(connectionFile: String, harness: String, sessionId: String, callerDirectory: String) {
        self.connectionFile = connectionFile
        self.harness = harness
        self.sessionId = sessionId
        self.callerDirectory = callerDirectory
    }

    func close() {
        client?.close()
        client = nil
        alfonsoRoute = nil
    }

    func alfonsoCallBlocking(_ method: String, _ params: [String: Any]) throws -> Any {
        var merged = params
        merged["harness"] = harness
        merged["sessionId"] = sessionId
        merged["callerDirectory"] = callerDirectory
        let (client, route) = try ensureAlfonsoBlocking()
        let reply = try client.callManagement(route: route, method: method, params: merged)
        guard let result = reply["result"] else {
            throw SubcError(message: "\(method): reply had no result field")
        }
        return JSONKeyNormalizer.camelize(result)
    }

    func pendingAsksBlocking() throws -> [AskRequest] {
        // This fleet-wide query intentionally has no recipient or session filter.
        let raw = try alfonsoCallBlocking("ask.list_pending_for_user", [:])
        return try decode([AskRequest].self, from: rowsArray(raw, key: "asks"))
    }

    func askBlocking(requestID: String) throws -> AskRequest? {
        let raw = try alfonsoCallBlocking("ask.get", ["requestID": requestID])
        if raw is NSNull { return nil }
        return try decode(AskRequest.self, from: raw)
    }

    func refreshAfterMutationBlocking(requestID: String) -> MutationRefresh {
        var detail: AskRequest?
        var asks: [AskRequest]?
        var errors: [String] = []

        do {
            detail = try askBlocking(requestID: requestID)
        } catch {
            errors.append("detail refresh failed: \(shortError(error))")
        }
        do {
            asks = try pendingAsksBlocking()
        } catch {
            errors.append("list refresh failed: \(shortError(error))")
        }
        return MutationRefresh(detail: detail, asks: asks, errorMessage: errors.isEmpty ? nil : errors.joined(separator: " · "))
    }

    private func ensureAlfonsoBlocking() throws -> (SubcClient, RouteHandle) {
        if let client, let alfonsoRoute { return (client, alfonsoRoute) }
        let c = try SubcClient.connect(connectionFilePath: connectionFile)
        let route = try c.routeOpenManagementSurface(
            moduleId: "alfonso-core",
            projectRoot: callerDirectory,
            harness: harness,
            session: sessionId)
        client = c
        alfonsoRoute = route
        return (c, route)
    }

    private func decode<T: Decodable>(_ type: T.Type, from any: Any) throws -> T {
        let data = try JSONSerialization.data(withJSONObject: any)
        return try JSONDecoder().decode(T.self, from: data)
    }

    /// List results may arrive as a bare array or wrapped under the operation's key.
    private func rowsArray(_ any: Any, key: String) -> Any {
        if let dict = any as? [String: Any], let inner = dict[key] { return inner }
        return any
    }
}

private func validateAskDismissReply(_ raw: Any) throws {
    guard let reply = raw as? [String: Any], let ok = reply["ok"] as? Bool, !ok else { return }
    let code = reply["code"] as? String ?? "unsuccessful reply"
    throw SubcError(message: "ask.resolve_user_ask: \(code)")
}

private struct MutationRefresh {
    var detail: AskRequest?
    var asks: [AskRequest]?
    var errorMessage: String?
}
