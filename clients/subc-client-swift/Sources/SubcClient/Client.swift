import Foundation

// Consumer-side channel-0 control RPC + session data/stream plane over one
// authenticated transport. This client is synchronous, but every route identity
// still carries the full wire generation and connection token.

public struct CatalogEntry {
    public let moduleId: String
    public let roles: [String]
    public let controlOps: [String]
}

/// One decoded subscribe-stream control event (the chat-renderable unit).
public struct SessionEvent {
    public let walSeq: UInt64
    public let subIndex: UInt32
    public let type: String
    public let text: String?
    public let runId: String?
    public let errorClass: String?
    public let errorStatus: Int?
    public let finishReason: String?
    /// Durable CONTROL units carry cursors; lossy DISPLAY events do not.
    public let isControl: Bool
}

/// A durable resubscribe position: replay strictly AFTER this (wal_seq, sub_index).
public typealias SubscribeCursor = (walSeq: UInt64, subIndex: UInt32)

public struct SubcError: Error {
    public let message: String
    public init(message: String) { self.message = message }
}

public enum ClientLocalError: Error, Equatable, CustomStringConvertible {
    case staleConnectionToken
    case routeNotLive(channel: UInt16, epoch: UInt32)
    case illegalAdmissionClass(AdmissionClass, FrameType)
    case correlationExhausted

    public var description: String {
        switch self {
        case .staleConnectionToken:
            "route handle belongs to a different connection"
        case let .routeNotLive(channel, epoch):
            "route handle is not live: channel=\(channel), epoch=\(epoch)"
        case let .illegalAdmissionClass(admissionClass, frameType):
            "admission class \(admissionClass) is illegal for frame type \(frameType)"
        case .correlationExhausted:
            "connection correlation space is exhausted"
        }
    }
}

final class ConnectionToken: @unchecked Sendable {}

/// Immutable route identity. The connection token is intentionally opaque and
/// never appears on the wire; it prevents a pre-reconnect handle from acting on
/// a later socket that happens to reuse the same channel and epoch numbers.
public struct RouteHandle: Hashable, Sendable {
    public let channel: UInt16
    public let epoch: UInt32
    fileprivate let connectionToken: ConnectionToken

    init(channel: UInt16, epoch: UInt32, connectionToken: ConnectionToken) {
        self.channel = channel
        self.epoch = epoch
        self.connectionToken = connectionToken
    }

    public static func == (lhs: RouteHandle, rhs: RouteHandle) -> Bool {
        lhs.channel == rhs.channel
            && lhs.epoch == rhs.epoch
            && lhs.connectionToken === rhs.connectionToken
    }

    public func hash(into hasher: inout Hasher) {
        hasher.combine(channel)
        hasher.combine(epoch)
        hasher.combine(ObjectIdentifier(connectionToken))
    }
}

private struct InFlightKey: Hashable {
    let channel: UInt16
    let epoch: UInt32
    let corr: UInt64
}

public final class SubcClient {
    private let transport: Transport
    private let connectionToken = ConnectionToken()
    private var liveEpochs: [UInt16: UInt32] = [:]
    private var nextCorr: UInt64
    private var corrExhausted = false
    private var inFlight: Set<InFlightKey> = []
    private var abandonedRouteOpens: Set<UInt64> = []
    public private(set) var droppedIngressFrames: UInt64 = 0

    init(transport: Transport, nextCorr: UInt64 = 1) {
        self.transport = transport
        self.nextCorr = nextCorr == 0 ? 1 : nextCorr
    }

    /// Read the connection file, connect to the first endpoint, run the HMAC
    /// handshake, and return a ready consumer client.
    public static func connect(connectionFilePath: String) throws -> SubcClient {
        let conn = try readConnectionFile(connectionFilePath)
        guard let endpoint = conn.endpoints.first else {
            throw SubcError(message: "connection file has no endpoints")
        }
        let transport = try POSIXTransport(host: endpoint.host, port: endpoint.port)
        try authenticateClient(transport, conn)
        return SubcClient(transport: transport)
    }

    /// List modules subc knows about (channel-0 catalog.list).
    public func catalogList(admissionClass: AdmissionClass = .normal) throws -> [CatalogEntry] {
        let body = try JSONSerialization.data(withJSONObject: ["op": "catalog.list"])
        let reply = try controlRequest(body: body, admissionClass: admissionClass)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
            throw SubcError(message: "catalog.list reply was not a JSON object")
        }
        let modules = obj["modules"] as? [[String: Any]] ?? []
        return modules.map { module in
            CatalogEntry(
                moduleId: module["module_id"] as? String ?? "?",
                roles: rolesOf(module["roles"]),
                controlOps: module["control_ops"] as? [String] ?? []
            )
        }
    }

    /// Fetch a tool provider's definitions in broca's `session.send` shape.
    public func toolProviderTools(
        moduleId: String,
        admissionClass: AdmissionClass = .normal
    ) throws -> [[String: Any]] {
        let body = try JSONSerialization.data(withJSONObject: ["op": "catalog.list"])
        let reply = try controlRequest(body: body, admissionClass: admissionClass)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any],
              let modules = obj["modules"] as? [[String: Any]],
              let entry = modules.first(where: { ($0["module_id"] as? String) == moduleId })
        else { return [] }
        let roles = entry["roles"] as? [[String: Any]] ?? []
        guard let provider = roles.first(where: { ($0["role"] as? String) == "tool_provider" }),
              let tools = provider["tools"] as? [[String: Any]]
        else { return [] }
        return tools.compactMap { tool in
            guard let name = tool["name"] as? String else { return nil }
            return [
                "name": name,
                "description": tool["description"] as? String ?? "",
                "input_schema": tool["schema"] as? [String: Any] ?? ["type": "object"],
                "module": moduleId,
            ]
        }
    }

    /// Open a management-surface route and publish its full handle before any
    /// subsequent ingress frame can be read.
    public func routeOpenManagementSurface(
        moduleId: String,
        projectRoot: String,
        harness: String,
        session: String,
        consumerCapabilities: [String]? = nil,
        admissionClass: AdmissionClass = .normal
    ) throws -> RouteHandle {
        try validateRequestAdmission(admissionClass)
        var payload: [String: Any] = [
            "op": "route.open",
            "target": ["kind": "management_surface", "module_id": moduleId],
            "identity": ["project_root": projectRoot, "harness": harness, "session": session],
        ]
        if let consumerCapabilities {
            payload["consumer_capabilities"] = consumerCapabilities
        }
        let body = try JSONSerialization.data(withJSONObject: payload)
        let corr = try allocateCorr()
        let key = InFlightKey(channel: 0, epoch: 0, corr: corr)
        inFlight.insert(key)
        defer { inFlight.remove(key) }
        try writeControlRequest(corr: corr, body: body, admissionClass: admissionClass)

        while true {
            let frame: Frame
            do {
                frame = try nextIngressFrame()
            } catch {
                // A local read timeout/failure can race a daemon-side commit. Retain
                // the corr so a later successful response becomes an immediate
                // GOODBYE instead of orphaning the route.
                abandonedRouteOpens.insert(corr)
                throw error
            }
            guard frame.header.channel == 0, frame.header.epoch == 0,
                  frame.header.corr == corr
            else { continue }
            switch frame.header.ty {
            case .response:
                // Installation occurs in this dispatch turn, before return and
                // before the socket reader asks for another frame.
                return try installRouteOpen(frame.body)
            case .error:
                throw remoteError(prefix: "route.open rejected", body: frame.body)
            default:
                continue
            }
        }
    }

    /// Invoke one management operation using the generation-fenced route handle.
    public func callManagement(
        route: RouteHandle,
        method: String,
        params: [String: Any] = [:],
        admissionClass: AdmissionClass = .normal
    ) throws -> [String: Any] {
        let body = try JSONSerialization.data(withJSONObject: ["method": method, "params": params])
        let reply = try routeRequest(route: route, body: body, admissionClass: admissionClass)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
            throw SubcError(message: "\(method) reply was not a JSON object")
        }
        return obj
    }

    /// Send a route-scoped CANCEL. The caller supplies the corr of the operation
    /// being cancelled; the handle supplies the immutable route generation.
    public func cancel(
        route: RouteHandle,
        corr: UInt64,
        admissionClass: AdmissionClass = .normal
    ) throws {
        try ensureCurrent(route)
        try validateAdmission(admissionClass, for: .cancel)
        let flags = buildFlags(
            binary: false,
            priority: .interactive,
            last: false,
            admissionClass: admissionClass
        )
        try transport.writeAll(encodeFrame(
            ty: .cancel,
            flags: flags,
            channel: route.channel,
            epoch: route.epoch,
            corr: corr,
            body: Data()
        ))
    }

    /// Close one live route. A stale or foreign handle fails before any write.
    public func closeRoute(
        _ route: RouteHandle,
        admissionClass: AdmissionClass = .normal
    ) throws {
        try ensureCurrent(route)
        try validateAdmission(admissionClass, for: .goodbye)
        try writeGoodbye(route, admissionClass: admissionClass)
        liveEpochs.removeValue(forKey: route.channel)
    }

    /// Drive one broca session turn end-to-end using dedicated command and
    /// subscribe handles.
    public func runSessionTurn(
        moduleId: String,
        projectRoot: String,
        harness: String,
        session: String,
        prompt: String,
        provider: String,
        model: String,
        tools: [[String: Any]] = [],
        fromCursor: SubscribeCursor? = nil,
        sendId: String = UUID().uuidString,
        admissionClass: AdmissionClass = .normal,
        onEvent: (SessionEvent) -> Void
    ) throws -> SubscribeCursor? {
        let commandRoute = try routeOpenManagementSurface(
            moduleId: moduleId,
            projectRoot: projectRoot,
            harness: harness,
            session: session,
            admissionClass: admissionClass
        )
        let subscribeRoute = try routeOpenManagementSurface(
            moduleId: moduleId,
            projectRoot: projectRoot,
            harness: harness,
            session: session,
            admissionClass: admissionClass
        )

        let subscribeCorr = try allocateCorr()
        let fromValue: Any
        if let cursor = fromCursor {
            fromValue = ["wal_seq": cursor.walSeq, "sub_index": cursor.subIndex]
        } else {
            fromValue = "start"
        }
        let subscribeBody = try JSONSerialization.data(withJSONObject: [
            "method": "session.subscribe",
            "params": ["from": fromValue],
        ])
        try beginRouteRequest(
            route: subscribeRoute,
            corr: subscribeCorr,
            body: subscribeBody,
            admissionClass: admissionClass
        )

        let sendCorr = try allocateCorr()
        let sendBody = try JSONSerialization.data(withJSONObject: [
            "method": "session.send",
            "params": [
                "prompt": prompt,
                "model": ["provider": provider, "model": model],
                "tools": tools,
                "send_id": sendId,
            ],
        ])
        try beginRouteRequest(
            route: commandRoute,
            corr: sendCorr,
            body: sendBody,
            admissionClass: admissionClass
        )

        let subscribeKey = InFlightKey(
            channel: subscribeRoute.channel,
            epoch: subscribeRoute.epoch,
            corr: subscribeCorr
        )
        let sendKey = InFlightKey(
            channel: commandRoute.channel,
            epoch: commandRoute.epoch,
            corr: sendCorr
        )
        inFlight.formUnion([subscribeKey, sendKey])
        defer {
            inFlight.remove(subscribeKey)
            inFlight.remove(sendKey)
        }

        var lastCursor: SubscribeCursor? = fromCursor
        while true {
            let frame = try nextIngressFrame()
            let frameHandle = (frame.header.channel, frame.header.epoch)
            if frame.header.ty == .goodbye,
               frameHandle == (subscribeRoute.channel, subscribeRoute.epoch)
                || frameHandle == (commandRoute.channel, commandRoute.epoch)
            {
                throw SubcError(message:
                    "route closed by daemon mid-turn (module restarted or drained) — resend to reopen")
            }

            let key = InFlightKey(
                channel: frame.header.channel,
                epoch: frame.header.epoch,
                corr: frame.header.corr
            )
            guard inFlight.contains(key) else { continue }
            if key == sendKey {
                if frame.header.ty == .error {
                    throw remoteError(prefix: "session.send rejected", body: frame.body)
                }
                if frame.header.ty == .response { inFlight.remove(sendKey) }
                continue
            }
            guard key == subscribeKey else { continue }
            switch frame.header.ty {
            case .streamData:
                if let event = try decodeStreamEvent(frame.body) {
                    if event.isControl { lastCursor = (event.walSeq, event.subIndex) }
                    onEvent(event)
                    if event.type == "run_finished" { return lastCursor }
                }
            case .streamEnd:
                return lastCursor
            case .error:
                throw remoteError(prefix: "subscribe stream error", body: frame.body)
            default:
                continue
            }
        }
    }

    /// Close the connection and invalidate every handle minted by it.
    public func close() {
        liveEpochs.removeAll()
        transport.close()
    }

    private func controlRequest(body: Data, admissionClass: AdmissionClass) throws -> Data {
        try validateRequestAdmission(admissionClass)
        let corr = try allocateCorr()
        let key = InFlightKey(channel: 0, epoch: 0, corr: corr)
        inFlight.insert(key)
        defer { inFlight.remove(key) }
        try writeControlRequest(corr: corr, body: body, admissionClass: admissionClass)
        while true {
            let frame = try nextIngressFrame()
            guard frame.header.channel == 0, frame.header.epoch == 0,
                  frame.header.corr == corr
            else { continue }
            switch frame.header.ty {
            case .response:
                return frame.body
            case .error:
                throw remoteError(prefix: "control request rejected", body: frame.body)
            default:
                continue
            }
        }
    }

    private func routeRequest(
        route: RouteHandle,
        body: Data,
        admissionClass: AdmissionClass
    ) throws -> Data {
        try ensureCurrent(route)
        try validateRequestAdmission(admissionClass)
        let corr = try allocateCorr()
        let key = InFlightKey(channel: route.channel, epoch: route.epoch, corr: corr)
        inFlight.insert(key)
        defer { inFlight.remove(key) }
        try beginRouteRequest(
            route: route,
            corr: corr,
            body: body,
            admissionClass: admissionClass
        )
        while true {
            let frame = try nextIngressFrame()
            let frameKey = InFlightKey(
                channel: frame.header.channel,
                epoch: frame.header.epoch,
                corr: frame.header.corr
            )
            if frame.header.ty == .goodbye,
               frame.header.channel == route.channel,
               frame.header.epoch == route.epoch
            {
                throw SubcError(message: "route closed while request was in flight")
            }
            guard frameKey == key else { continue }
            switch frame.header.ty {
            case .response:
                return frame.body
            case .error:
                throw remoteError(
                    prefix: "request on route \(route.channel):\(route.epoch) rejected",
                    body: frame.body
                )
            default:
                continue
            }
        }
    }

    private func beginRouteRequest(
        route: RouteHandle,
        corr: UInt64,
        body: Data,
        admissionClass: AdmissionClass
    ) throws {
        try ensureCurrent(route)
        try validateRequestAdmission(admissionClass)
        let flags = buildFlags(
            binary: false,
            priority: .interactive,
            last: false,
            admissionClass: admissionClass
        )
        try transport.writeAll(encodeFrame(
            ty: .request,
            flags: flags,
            channel: route.channel,
            epoch: route.epoch,
            corr: corr,
            body: body
        ))
    }

    private func writeControlRequest(
        corr: UInt64,
        body: Data,
        admissionClass: AdmissionClass
    ) throws {
        let flags = buildFlags(
            binary: false,
            priority: .interactive,
            last: false,
            admissionClass: admissionClass
        )
        try transport.writeAll(encodeFrame(
            ty: .request,
            flags: flags,
            channel: 0,
            epoch: 0,
            corr: corr,
            body: body
        ))
    }

    private func nextIngressFrame() throws -> Frame {
        while true {
            let frame = try readFrame(from: transport)
            if frame.header.channel == 0 {
                if try consumeLateRouteOpen(frame) { continue }
                return frame
            }
            guard liveEpochs[frame.header.channel] == frame.header.epoch else {
                droppedIngressFrames &+= 1
                continue
            }
            if frame.header.ty == .goodbye {
                liveEpochs.removeValue(forKey: frame.header.channel)
            }
            return frame
        }
    }

    private func consumeLateRouteOpen(_ frame: Frame) throws -> Bool {
        guard abandonedRouteOpens.contains(frame.header.corr),
              frame.header.channel == 0, frame.header.epoch == 0
        else { return false }
        guard frame.header.ty == .response || frame.header.ty == .error else { return false }
        abandonedRouteOpens.remove(frame.header.corr)
        guard frame.header.ty == .response else { return true }

        let route = try installRouteOpen(frame.body)
        do {
            try writeGoodbye(route, admissionClass: .normal)
            liveEpochs.removeValue(forKey: route.channel)
        } catch {
            // A committed late route must not remain orphaned if its GOODBYE
            // cannot be queued; owner cleanup on connection close is the floor.
            close()
            throw error
        }
        return true
    }

    private func installRouteOpen(_ body: Data) throws -> RouteHandle {
        guard let object = try JSONSerialization.jsonObject(with: body) as? [String: Any],
              let channelNumber = object["route_channel"] as? NSNumber,
              let epochNumber = object["route_epoch"] as? NSNumber,
              channelNumber.int64Value > 0,
              channelNumber.uint64Value <= UInt64(UInt16.max),
              epochNumber.int64Value > 0,
              epochNumber.uint64Value <= UInt64(UInt32.max)
        else {
            throw SubcError(message: "route.open returned no valid route_channel/route_epoch")
        }
        let route = RouteHandle(
            channel: UInt16(channelNumber.uint64Value),
            epoch: UInt32(epochNumber.uint64Value),
            connectionToken: connectionToken
        )
        liveEpochs[route.channel] = route.epoch
        return route
    }

    private func ensureCurrent(_ route: RouteHandle) throws {
        guard route.connectionToken === connectionToken else {
            throw ClientLocalError.staleConnectionToken
        }
        guard liveEpochs[route.channel] == route.epoch else {
            throw ClientLocalError.routeNotLive(channel: route.channel, epoch: route.epoch)
        }
    }

    private func validateRequestAdmission(_ admissionClass: AdmissionClass) throws {
        try validateAdmission(admissionClass, for: .request)
    }

    private func validateAdmission(_ admissionClass: AdmissionClass, for frameType: FrameType) throws {
        if admissionClass == .sheddable,
           frameType != .push, frameType != .streamData
        {
            throw ClientLocalError.illegalAdmissionClass(admissionClass, frameType)
        }
    }

    private func writeGoodbye(
        _ route: RouteHandle,
        admissionClass: AdmissionClass
    ) throws {
        let flags = buildFlags(
            binary: false,
            priority: .passive,
            last: false,
            admissionClass: admissionClass
        )
        try transport.writeAll(encodeFrame(
            ty: .goodbye,
            flags: flags,
            channel: route.channel,
            epoch: route.epoch,
            corr: 0,
            body: Data()
        ))
    }

    private func allocateCorr() throws -> UInt64 {
        guard !corrExhausted else {
            close()
            throw ClientLocalError.correlationExhausted
        }
        let corr = nextCorr
        if corr == UInt64.max {
            corrExhausted = true
        } else {
            nextCorr += 1
        }
        return corr
    }

    private func remoteError(prefix: String, body: Data) -> SubcError {
        let message = String(data: body, encoding: .utf8) ?? "<binary>"
        return SubcError(message: "\(prefix): \(message)")
    }
}

// Decode a subscribe StreamData body into a renderable SessionEvent. Two shapes:
//   control: { kind:"control", cursor:{wal_seq, sub_index}, unit:{type,...} }  (durable)
//   display: { kind:"display", event:{type:"text_delta", delta, ...} }         (live, lossy)
private func decodeStreamEvent(_ body: Data) throws -> SessionEvent? {
    guard let v = try JSONSerialization.jsonObject(with: body) as? [String: Any] else {
        throw SubcError(message: "subscribe event not a JSON object")
    }
    if (v["kind"] as? String) == "display" {
        return decodeDisplayEvent(v)
    }
    guard (v["kind"] as? String) == "control" else { return nil }
    let cursor = v["cursor"] as? [String: Any]
    let walSeq = (cursor?["wal_seq"] as? Int).map { UInt64($0) } ?? 0
    let subIndex = (cursor?["sub_index"] as? Int).map { UInt32($0) } ?? 0
    guard let unit = v["unit"] as? [String: Any], let type = unit["type"] as? String else {
        throw SubcError(message: "control event missing unit.type")
    }
    let runId: String? = type == "run_started" ? (unit["run_id"] as? String) : nil

    // A terminal typed error: { type: "error", error: { class, message, status?, ... } }.
    // The wire carries this on the control lane; surfacing it is the client's job (an
    // unrendered error is the difference between a blank bubble and a real diagnosis).
    var text = extractText(type: type, unit: unit)
    var errorClass: String? = nil
    var errorStatus: Int? = nil
    if type == "error", let err = unit["error"] as? [String: Any] {
        text = err["message"] as? String
        errorClass = err["class"] as? String
        errorStatus = err["status"] as? Int
    }
    // run_finished carries the terminal reason (snake_case); a non-`completed` reason means
    // the run failed/stopped without producing a normal answer.
    let finishReason = type == "run_finished" ? (unit["reason"] as? String) : nil
    return SessionEvent(
        walSeq: walSeq, subIndex: subIndex, type: type, text: text,
        runId: runId, errorClass: errorClass, errorStatus: errorStatus,
        finishReason: finishReason, isControl: true)
}

// Decode a live display event { kind:"display", event:{ type, delta, ... } }. Only
// text_delta carries renderable assistant text; reasoning/tool-input deltas and the
// gap/reset markers are surfaced by type with no text so the view can ignore them.
private func decodeDisplayEvent(_ v: [String: Any]) -> SessionEvent? {
    guard let event = v["event"] as? [String: Any], let type = event["type"] as? String else {
        return nil
    }
    let text = type == "text_delta" ? (event["delta"] as? String) : nil
    return SessionEvent(
        walSeq: 0, subIndex: 0, type: type, text: text,
        runId: nil, errorClass: nil, errorStatus: nil, finishReason: nil, isControl: false)
}

private func extractText(type: String, unit: [String: Any]) -> String? {
    switch type {
    case "assistant_message":
        let content = (unit["message"] as? [String: Any])?["content"] as? [[String: Any]] ?? []
        let text = content.compactMap { block -> String? in
            (block["type"] as? String) == "text" ? block["text"] as? String : nil
        }.joined()
        return text.isEmpty ? nil : text
    case "tool_call":
        // Render as `name(compact-args)` so the transcript shows what ran. The
        // input can be large (e.g. a write); cap it for the caption.
        if let call = unit["call"] as? [String: Any], let name = call["tool_name"] as? String {
            var args = ""
            if let input = call["input"],
               let data = try? JSONSerialization.data(withJSONObject: input),
               let s = String(data: data, encoding: .utf8) {
                args = s.count > 200 ? String(s.prefix(200)) + "…" : s
            }
            return "\(name)(\(args))"
        }
        return nil
    case "tool_result":
        return (unit["result"] as? [String: Any])?["output"].flatMap { ($0 as? [String: Any])?["text"] as? String }
    default:
        return nil
    }
}

private func rolesOf(_ value: Any?) -> [String] {
    // Each ProviderRole serializes as an internally-tagged object
    // { "role": "management_surface", ... } (serde tag = "role").
    if let objs = value as? [[String: Any]] {
        return objs.compactMap { $0["role"] as? String }
    }
    return []
}
