import Foundation

// Consumer-side channel-0 control RPC + the session data/stream plane over a
// connected + authenticated transport. Mirrors clients/subc-client/src/client.ts,
// subc-probe.rs, and llm-runner's llmr-subc SubcConnection.
//
// Spike scope: a synchronous, single-socket demonstration. The production client
// swaps the blocking transport for an async Network.framework connection with a
// background read task and per-(channel, corr) registration, which is what lets
// subscribe streams and concurrent in-flight requests share one connection. The
// wire bytes (frames, bodies, demux key) are identical; only the I/O model changes.

public struct CatalogEntry {
    public let moduleId: String
    public let roles: [String]
    public let controlOps: [String]
}

/// One decoded subscribe-stream control event (the chat-renderable unit).
public struct SessionEvent {
    public let walSeq: UInt64
    public let subIndex: UInt32
    public let type: String        // run_started | step_started | assistant_message | tool_call | tool_result | step_finished | run_finished | error | text_delta | ...
    public let text: String?       // assistant text (assistant_message), tool result text, the error message (error), or a live token delta (text_delta)
    public let runId: String?      // present on run_started (the run this episode belongs to)
    public let errorClass: String? // present on type=="error": transient | permanent | auth | context_overflow | provider_unavailable
    public let errorStatus: Int?   // present on type=="error" when the provider supplied an HTTP status
    public let finishReason: String? // present on type=="run_finished": completed | max_steps | cancelled | interrupted | error
    /// True for a durable CONTROL unit (carries a cursor); false for a live DISPLAY event
    /// (token delta, lossy, no cursor). Only control events advance the resubscribe cursor.
    public let isControl: Bool
}

/// A durable resubscribe position: replay strictly AFTER this (wal_seq, sub_index).
public typealias SubscribeCursor = (walSeq: UInt64, subIndex: UInt32)

public struct SubcError: Error { public let message: String }

public final class SubcClient {
    private let transport: Transport
    private var nextCorr: UInt64 = 1

    private init(transport: Transport) {
        self.transport = transport
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
    public func catalogList() throws -> [CatalogEntry] {
        let body = try JSONSerialization.data(withJSONObject: ["op": "catalog.list"])
        let reply = try request(channel: 0, body: body)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
            throw SubcError(message: "catalog.list reply was not a JSON object")
        }
        let modules = obj["modules"] as? [[String: Any]] ?? []
        return modules.map { m in
            CatalogEntry(
                moduleId: m["module_id"] as? String ?? "?",
                roles: rolesOf(m["roles"]),
                controlOps: m["control_ops"] as? [String] ?? []
            )
        }
    }

    /// Open a route to a management-surface module (channel-0 route.open).
    /// Returns the assigned route channel. `projectRoot` must be an existing path
    /// (subc canonicalizes it via cortexkit-paths and rejects non-existent paths).
    public func routeOpenManagementSurface(
        moduleId: String,
        projectRoot: String,
        harness: String,
        session: String
    ) throws -> UInt16 {
        let body = try JSONSerialization.data(withJSONObject: [
            "op": "route.open",
            "target": ["kind": "management_surface", "module_id": moduleId],
            "identity": ["project_root": projectRoot, "harness": harness, "session": session],
        ])
        let reply = try request(channel: 0, body: body)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any],
              let ch = obj["route_channel"] as? Int else {
            throw SubcError(message: "route.open returned no route_channel")
        }
        return UInt16(ch)
    }

    /// Invoke a management operation on an open route channel and return the
    /// decoded JSON result (unwrapping the serve side's `{ result: ... }` envelope).
    public func callManagement(routeChannel: UInt16, method: String, params: [String: Any] = [:]) throws -> [String: Any] {
        let body = try JSONSerialization.data(withJSONObject: ["method": method, "params": params])
        let reply = try request(channel: routeChannel, body: body)
        guard let obj = try JSONSerialization.jsonObject(with: reply) as? [String: Any] else {
            throw SubcError(message: "\(method) reply was not a JSON object")
        }
        return obj
    }

    /// Drive one llm-runner session turn end-to-end: subscribe (dedicated route),
    /// send the prompt (command route), then drain the control stream to the run's
    /// terminal, invoking `onEvent` for each decoded control unit.
    ///
    /// This is the spike-level synchronous proof of subscribe streaming: a subscribe
    /// is a Request whose corr yields a series of StreamData frames (one per control
    /// unit) ending in StreamEnd, distinct from a request/response's single terminal.
    /// Subscribing BEFORE sending captures the run from seq 1 (the attach barrier
    /// replays the projection from start). The read loop demultiplexes by corr: the
    /// send's Response vs the subscribe's StreamData stream.
    public func runSessionTurn(
        moduleId: String,
        projectRoot: String,
        harness: String,
        session: String,
        prompt: String,
        provider: String,
        model: String,
        fromCursor: SubscribeCursor? = nil,
        appendEpisode: Bool = false,
        onEvent: (SessionEvent) -> Void
    ) throws -> SubscribeCursor? {
        let cmdChannel = try routeOpenManagementSurface(moduleId: moduleId, projectRoot: projectRoot, harness: harness, session: session)
        let subChannel = try routeOpenManagementSurface(moduleId: moduleId, projectRoot: projectRoot, harness: harness, session: session)

        // Subscribe FIRST (from the start of the lineage), without waiting — its corr
        // produces the StreamData stream we drain below.
        let subCorr = nextCorr; nextCorr += 1
        // A continuing turn resubscribes from the prior turn's last cursor (replays
        // strictly AFTER it), so the prior episode is never re-delivered; the first
        // turn attaches from "start" on the empty lineage. The `from` wire shape is an
        // untagged enum: a bare string ("start"/"live") or a { wal_seq, sub_index } object.
        let fromValue: Any
        if let c = fromCursor {
            fromValue = ["wal_seq": c.walSeq, "sub_index": c.subIndex]
        } else {
            fromValue = "start"
        }
        let subBody = try JSONSerialization.data(withJSONObject: [
            "method": "session.subscribe",
            "params": ["from": fromValue],
        ])
        try writeRequest(channel: subChannel, corr: subCorr, body: subBody)

        // Then send the prompt on the command route (its own corr → one Response).
        let sendCorr = nextCorr; nextCorr += 1
        let sendBody = try JSONSerialization.data(withJSONObject: [
            "method": "session.send",
            "params": [
                "prompt": prompt,
                "model": ["provider": provider, "model": model],
                "tools": [],
                "append_episode": appendEpisode,
            ],
        ])
        try writeRequest(channel: cmdChannel, corr: sendCorr, body: sendBody)

        // Drain: demux frames by corr. The send Response is the admission ack; the
        // subscribe corr carries the StreamData control units until the run's terminal.
        var lastCursor: SubscribeCursor? = fromCursor
        while true {
            let frame = try readFrame()
            if frame.header.corr == sendCorr {
                if frame.header.ty == .error {
                    let msg = String(data: frame.body, encoding: .utf8) ?? "<binary>"
                    throw SubcError(message: "session.send rejected: \(msg)")
                }
                continue // Response = admission ack; the stream carries the run
            }
            guard frame.header.corr == subCorr else { continue }
            switch frame.header.ty {
            case .streamData:
                if let event = try decodeStreamEvent(frame.body) {
                    // Only durable CONTROL events carry a cursor and advance the resubscribe
                    // position; live DISPLAY deltas are lossy and must not move the cursor.
                    if event.isControl { lastCursor = (event.walSeq, event.subIndex) }
                    onEvent(event)
                    if event.type == "run_finished" { return lastCursor }
                }
            case .streamEnd:
                return lastCursor
            case .error:
                let msg = String(data: frame.body, encoding: .utf8) ?? "<binary>"
                throw SubcError(message: "subscribe stream error: \(msg)")
            default:
                continue // interim push, ignore
            }
        }
    }

    public func close() { transport.close() }

    // Send a Request on `channel` and read frames until the terminal
    // (Response/Error) carrying THIS request's corr. Frames for other correlations
    // are skipped — the demux-by-corr discipline the production client generalizes
    // to full (channel, corr) keying for concurrent in-flight requests.
    private func request(channel: UInt16, body: Data) throws -> Data {
        let corr = nextCorr; nextCorr += 1
        try writeRequest(channel: channel, corr: corr, body: body)
        while true {
            let frame = try readFrame()
            guard frame.header.corr == corr else { continue }
            switch frame.header.ty {
            case .response:
                return frame.body
            case .error:
                let msg = String(data: frame.body, encoding: .utf8) ?? "<binary>"
                throw SubcError(message: "request on channel \(channel) rejected: \(msg)")
            default:
                continue
            }
        }
    }

    private func writeRequest(channel: UInt16, corr: UInt64, body: Data) throws {
        let flags = buildFlags(binary: false, priority: .interactive, last: false)
        try transport.writeAll(encodeFrame(ty: .request, flags: flags, channel: channel, corr: corr, body: body))
    }

    private func readFrame() throws -> Frame {
        let header = try decodeHeader(try transport.readExact(HEADER_LEN))
        let body = header.len > 0 ? try transport.readExact(Int(header.len)) : Data()
        return Frame(header: header, body: body)
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
