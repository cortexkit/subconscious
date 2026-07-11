import Foundation

// MARK: - Key normalization
//
// The observe/athena ops are pinned camelCase, but the rooms round proved wire
// casing can drift between a module's internal types and its serve layer
// (session_id vs sessionId cost a smoke-test round). Rather than dual-key every
// Codable, normalize snake_case keys to camelCase recursively on the raw JSON
// object before decoding, so either casing decodes.
enum JSONKeyNormalizer {
    static func camelize(_ any: Any) -> Any {
        if let dict = any as? [String: Any] {
            var out: [String: Any] = [:]
            out.reserveCapacity(dict.count)
            for (k, v) in dict {
                out[camelKey(k)] = camelize(v)
            }
            return out
        }
        if let arr = any as? [Any] {
            return arr.map { camelize($0) }
        }
        return any
    }

    private static func camelKey(_ key: String) -> String {
        guard key.contains("_") else { return key }
        var parts = key.split(separator: "_").map(String.init)
        guard let first = parts.first else { return key }
        parts.removeFirst()
        return first + parts.map { $0.isEmpty ? $0 : $0.prefix(1).uppercased() + $0.dropFirst() }.joined()
    }
}

// MARK: - Athena consult rows (alfonso-core athena.list_consults / athena.get_consult)

/// Row shape pinned from ALF's captured wire examples
/// (alfonso .cortexkit/alfonso/docs/observe-ops-wire-examples.md, 2026-07-11).
struct ConsultRow: Codable, Identifiable {
    var consultId: String
    var phase: String?
    var terminalReason: String?
    var consultClass: String?
    var questionPreview: String?
    var startedAtMs: Int64?
    var finishedAtMs: Int64?
    var ordinal: Int64?
    var memberRoutes: [String]?
    var sentinels: [String]?
    var evidenceCount: Int?
    var verdictCount: Int?

    enum CodingKeys: String, CodingKey {
        case consultId, phase, terminalReason, questionPreview, startedAtMs, finishedAtMs,
             ordinal, memberRoutes, sentinels, evidenceCount, verdictCount
        case consultClass = "class"
    }

    var id: String { consultId }
}

/// Attempt `model` is an object {provider, model} on the wire.
struct AttemptModel: Codable {
    var provider: String?
    var model: String?

    var label: String {
        switch (provider, model) {
        case let (.some(p), .some(m)): return "\(p)/\(m)"
        case let (nil, .some(m)): return m
        case let (.some(p), nil): return p
        default: return "member"
        }
    }
}

struct ConsultAttempt: Codable, Identifiable {
    var attemptId: String?
    var model: AttemptModel?
    var state: String?
    var phase: String?
    var round: Int?
    var sessionId: String?
    var projectRoot: String?
    var subjectKey: String?
    var startedAtMs: Int64?
    var settledAtMs: Int64?

    var id: String { attemptId ?? sessionId ?? UUID().uuidString }
}

/// The store keeps no durable phase-transition history; the wire returns this
/// honest current-phase tuple instead of a phaseHistory array.
struct CurrentPhase: Codable {
    var phase: String?
    var round: Int?
    var epoch: Int?
    var enteredAtMs: Int64?
}

struct SynthesisInfo: Codable {
    var present: Bool?
    var mechanical: Bool?
    var resultPreview: String?
    var sentinels: [String]?
}

struct EvidenceInfo: Codable {
    var count: Int?
    var unitKinds: [String: Int]?
}

struct ConsultDetail: Codable {
    var consultId: String
    var phase: String?
    var terminalReason: String?
    var questionPreview: String?
    var currentPhase: CurrentPhase?
    var attempts: [ConsultAttempt]?
    var evidence: EvidenceInfo?
    var evidenceCount: Int?
    var sentinels: [String]?
    var synthesis: SynthesisInfo?
    var memberRoutes: [String]?
    var startedAtMs: Int64?
    var finishedAtMs: Int64?
}

// MARK: - Recent runs (alfonso-core observe.recent_runs)

struct ObservedRun: Codable, Identifiable {
    var ordinal: Int64?
    var kind: String?
    var runKey: String?
    var sessionId: String?
    var projectRoot: String?
    var model: String?
    var startedAtMs: Int64?
    var finishedAtMs: Int64?
    var state: String?
    var preview: String?

    var id: String { runKey ?? sessionId ?? "\(ordinal ?? 0)" }
}

// MARK: - Broca transcript (session.read)

struct TranscriptMessage: Identifiable {
    var ordinal: Int64
    var role: String
    /// Flattened human-readable rendering of the canonical content blocks.
    var text: String
    /// Compact one-line descriptions of non-text blocks (tool calls/results, media).
    var blockSummaries: [String]

    var id: Int64 { ordinal }
}

struct LineageState {
    var lastRunId: String?
    var state: String?
    var reason: String?
    var errorText: String?
}

enum TranscriptDecoder {
    /// Decode a broca session.read response (already key-camelized) into renderable rows.
    ///
    /// Wire shape pinned from a live probe (2026-07-11): rows are
    /// `{ ordinal, message: { role, content: [block] } }`, blocks are keyed by
    /// `kind` (text | reasoning | toolCall | toolResult after camelization), and
    /// tool results nest their payload as `output: { kind, text }`. Tolerant by
    /// design: unknown block kinds render as compact JSON rather than failing,
    /// because the canonical Message vocabulary can grow (media, provider blocks).
    static func decode(_ result: [String: Any]) -> (messages: [TranscriptMessage], next: Int64?, lineage: LineageState?) {
        var rows: [TranscriptMessage] = []
        if let msgs = result["messages"] as? [[String: Any]] {
            for row in msgs {
                let ordinal = (row["ordinal"] as? NSNumber)?.int64Value ?? Int64(rows.count)
                // Envelope: { ordinal, message: {...} }; tolerate a flat shape too.
                let m = row["message"] as? [String: Any] ?? row
                let role = m["role"] as? String ?? "?"
                var texts: [String] = []
                var summaries: [String] = []
                if let blocks = m["content"] as? [[String: Any]] {
                    for b in blocks {
                        let ty = b["kind"] as? String ?? b["type"] as? String ?? "?"
                        switch ty {
                        case "text":
                            if let t = b["text"] as? String { texts.append(t) }
                        case "reasoning":
                            if let t = b["text"] as? String, !t.isEmpty {
                                summaries.append("[reasoning] \(String(t.prefix(160)))")
                            } else {
                                summaries.append("[reasoning]")
                            }
                        case "toolCall", "tool_call":
                            let name = b["name"] as? String ?? "?"
                            var args = ""
                            if let a = b["arguments"] ?? b["args"],
                               let data = try? JSONSerialization.data(withJSONObject: a),
                               let s = String(data: data, encoding: .utf8) {
                                args = " \(String(s.prefix(100)))"
                            }
                            summaries.append("[tool call] \(name)\(args)")
                        case "toolResult", "tool_result":
                            let isError = (b["isError"] as? Bool) ?? false
                            var payload = ""
                            if let out = b["output"] as? [String: Any], let t = out["text"] as? String {
                                payload = " \(String(t.prefix(160)))"
                            }
                            summaries.append("[tool result\(isError ? " ERROR" : "")]\(payload)")
                        default:
                            if let data = try? JSONSerialization.data(withJSONObject: b),
                               let s = String(data: data, encoding: .utf8) {
                                summaries.append("[\(ty)] \(String(s.prefix(120)))")
                            } else {
                                summaries.append("[\(ty)]")
                            }
                        }
                    }
                } else if let t = m["content"] as? String {
                    texts.append(t)
                }
                rows.append(TranscriptMessage(
                    ordinal: ordinal, role: role,
                    text: texts.joined(separator: "\n"),
                    blockSummaries: summaries))
            }
        }
        let next = (result["nextFromOrdinal"] as? NSNumber)?.int64Value
        var lineage: LineageState?
        if let ls = result["lineageState"] as? [String: Any] {
            var errorText: String?
            if let err = ls["error"] {
                if let s = err as? String {
                    errorText = s
                } else if let data = try? JSONSerialization.data(withJSONObject: err),
                          let s = String(data: data, encoding: .utf8) {
                    errorText = s
                }
            }
            lineage = LineageState(
                lastRunId: ls["lastRunId"] as? String,
                state: ls["state"] as? String,
                reason: ls["reason"] as? String,
                errorText: errorText)
        }
        return (rows, next, lineage)
    }
}
