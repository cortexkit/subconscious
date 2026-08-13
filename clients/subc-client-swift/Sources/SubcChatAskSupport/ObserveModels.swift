import Foundation

// MARK: - Key normalization
//
// The observe/athena ops are pinned camelCase, but the rooms round proved wire
// casing can drift between a module's internal types and its serve layer
// (session_id vs sessionId cost a smoke-test round). Rather than dual-key every
// Codable, normalize snake_case keys to camelCase recursively on the raw JSON
// object before decoding, so either casing decodes.
public enum JSONKeyNormalizer {
    public static func camelize(_ any: Any) -> Any {
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
public struct ConsultRow: Codable, Identifiable {
    public var consultId: String
    public var phase: String?
    public var terminalReason: String?
    public var consultClass: String?
    public var questionPreview: String?
    public var startedAtMs: Int64?
    public var finishedAtMs: Int64?
    public var ordinal: Int64?
    public var memberRoutes: [String]?
    public var sentinels: [String]?
    public var evidenceCount: Int?
    public var verdictCount: Int?
    /// Session that requested the consult, letting a client attribute each row to
    /// one agent. Absent for consults raised outside a session context, which
    /// belong to no single agent rather than to an unknown one.
    public var callerSession: String?

    public enum CodingKeys: String, CodingKey {
        case consultId, phase, terminalReason, questionPreview, startedAtMs, finishedAtMs,
             ordinal, memberRoutes, sentinels, evidenceCount, verdictCount, callerSession
        case consultClass = "class"
    }

    public var id: String { consultId }
}

/// Attempt `model` is an object {provider, model} on the wire.
public struct AttemptModel: Codable {
    public var provider: String?
    public var model: String?

    public var label: String {
        switch (provider, model) {
        case let (.some(p), .some(m)): return "\(p)/\(m)"
        case let (nil, .some(m)): return m
        case let (.some(p), nil): return p
        default: return "member"
        }
    }
}

public struct ConsultAttempt: Codable, Identifiable {
    public var attemptId: String?
    public var model: AttemptModel?
    public var state: String?
    public var phase: String?
    public var round: Int?
    public var sessionId: String?
    public var projectRoot: String?
    public var subjectKey: String?
    public var startedAtMs: Int64?
    public var settledAtMs: Int64?
    // Per-run usage for this stage send / member reply. Absent means the
    // provider reported nothing (render "unmeasured"); a present all-zero
    // object is a real measurement and renders as zeros.
    public var usage: AttemptUsage?

    public var id: String { attemptId ?? sessionId ?? UUID().uuidString }
}

public struct AttemptUsage: Codable {
    public var inputTokens: Int64?
    public var cachedInputTokens: Int64?
    public var cacheWriteTokens: Int64?
    public var outputTokens: Int64?
    public var reasoningTokens: Int64?
    public var retriesUsed: Int?
}

public enum TokenFormat {
    // Compact token counts for chip rendering: 999, 1.5k, 52k, 2.4M.
    public static func count(_ v: Int64) -> String {
        if v >= 1_000_000 { return String(format: "%.1fM", Double(v) / 1_000_000) }
        if v >= 10_000 { return String(format: "%.0fk", Double(v) / 1_000) }
        if v >= 1_000 { return String(format: "%.1fk", Double(v) / 1_000) }
        return "\(v)"
    }
}

public struct TokenUsageRollup: Codable {
    public var models: [TokenUsageModelRow]?
    public var total: TokenUsageModelRow?
}

public struct TokenUsageModelRow: Codable {
    public var model: String?
    public var calls: Int?
    public var unmeasured: Int?
    public var retriesUsed: Int?
    public var input: Int64?
    public var cachedInput: Int64?
    public var cacheWrite: Int64?
    public var output: Int64?
    public var reasoning: Int64?

    enum CodingKeys: String, CodingKey {
        case model, calls, unmeasured, retriesUsed
        case input, cachedInput, cacheWrite, output, reasoning
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        // The wire emits model as either a plain string or a structured
        // {provider, model} object depending on the row's origin; accept both.
        if let s = try? c.decodeIfPresent(String.self, forKey: .model) {
            model = s
        } else if let m = try? c.decodeIfPresent(AttemptModel.self, forKey: .model) {
            model = m.label
        } else {
            model = nil
        }
        calls = try c.decodeIfPresent(Int.self, forKey: .calls)
        unmeasured = try c.decodeIfPresent(Int.self, forKey: .unmeasured)
        retriesUsed = try c.decodeIfPresent(Int.self, forKey: .retriesUsed)
        input = try c.decodeIfPresent(Int64.self, forKey: .input)
        cachedInput = try c.decodeIfPresent(Int64.self, forKey: .cachedInput)
        cacheWrite = try c.decodeIfPresent(Int64.self, forKey: .cacheWrite)
        output = try c.decodeIfPresent(Int64.self, forKey: .output)
        reasoning = try c.decodeIfPresent(Int64.self, forKey: .reasoning)
    }
}

/// The store keeps no durable phase-transition history; the wire returns this
/// honest current-phase tuple instead of a phaseHistory array.
public struct CurrentPhase: Codable {
    public var phase: String?
    public var round: Int?
    public var epoch: Int?
    public var enteredAtMs: Int64?
}

public struct SynthesisInfo: Codable {
    public var present: Bool?
    public var mechanical: Bool?
    public var resultPreview: String?
    public var sentinels: [String]?
}

public struct EvidenceInfo: Codable {
    public var count: Int?
    public var unitKinds: [String: Int]?
}

public struct ConsultDetail: Codable {
    public var consultId: String
    public var phase: String?
    public var terminalReason: String?
    public var questionPreview: String?
    public var currentPhase: CurrentPhase?
    public var attempts: [ConsultAttempt]?
    public var evidence: EvidenceInfo?
    public var evidenceCount: Int?
    public var sentinels: [String]?
    public var synthesis: SynthesisInfo?
    public var memberRoutes: [String]?
    public var startedAtMs: Int64?
    public var finishedAtMs: Int64?
    public var tokenUsage: TokenUsageRollup?
}

// MARK: - Recent runs (alfonso-core observe.recent_runs)

public struct ObservedRun: Codable, Identifiable {
    public var ordinal: Int64?
    public var kind: String?
    public var runKey: String?
    public var sessionId: String?
    public var projectRoot: String?
    public var model: String?
    public var startedAtMs: Int64?
    public var finishedAtMs: Int64?
    public var state: String?
    public var preview: String?

    public var id: String { runKey ?? sessionId ?? "\(ordinal ?? 0)" }
}

// MARK: - Broca transcript (session.read)

public struct TranscriptMessage: Identifiable {
    public var ordinal: Int64
    public var role: String
    /// Flattened human-readable rendering of the canonical content blocks.
    public var text: String
    /// Compact one-line descriptions of non-text blocks (tool calls/results, media).
    public var blockSummaries: [String]

    public var id: Int64 { ordinal }
}

public struct LineageState {
    public var lastRunId: String?
    public var state: String?
    public var reason: String?
    public var errorText: String?
}

public enum TranscriptDecoder {
    /// Decode a broca session.read response (already key-camelized) into renderable rows.
    ///
    /// Wire shape pinned from a live probe (2026-07-11): rows are
    /// `{ ordinal, message: { role, content: [block] } }`, blocks are keyed by
    /// `kind` (text | reasoning | toolCall | toolResult after camelization), and
    /// tool results nest their payload as `output: { kind, text }`. Tolerant by
    /// design: unknown block kinds render as compact JSON rather than failing,
    /// because the canonical Message vocabulary can grow (media, provider blocks).
    public static func decode(_ result: [String: Any]) -> (messages: [TranscriptMessage], next: Int64?, lineage: LineageState?) {
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

// MARK: - Spec campaigns (athena.spec_status)

/// One auto-dispatched implementation slice in a spec campaign's ladder.
/// `dispatch` stays nil until a mason launches for the slice; `failureReason`
/// appears only when the dispatch reached a terminal-bad state.
public struct SpecSlice: Codable, Identifiable, Equatable {
    public var sliceId: String?
    public var title: String?
    public var status: String?
    public var updatedAtMs: Int64?
    public var verifyLeaf: SpecVerifyLeaf?
    public var dispatch: SpecDispatch?

    enum CodingKeys: String, CodingKey {
        case sliceId = "id"
        case title, status, updatedAtMs, verifyLeaf, dispatch
    }

    public var id: String { sliceId ?? title ?? UUID().uuidString }
}

public struct SpecVerifyLeaf: Codable, Equatable {
    public var id: String?
    public var status: String?
}

public struct SpecDispatch: Codable, Equatable {
    public var backgroundTaskId: String?
    public var taskState: String?
    public var scores: SpecScores?
    public var failureReason: String?
}

/// Slice evaluation scores. The producer (prefrontal `spec_status`) omits
/// the whole object while a slice is unscored, so `SpecDispatch.scores`
/// staying optional is load-bearing: "not scored yet" must remain
/// distinguishable from "scored", and absence must never flow into
/// arithmetic (an absent score coalesced to 0 renders a confident false
/// failing grade in the same style as a real one).
public struct SpecScores: Codable, Equatable {
    /// The current scoring axis, 0...100. The producer folds legacy
    /// `code_quality` into this key, so historical evaluations arrive
    /// here too.
    public var workQuality: Int?
    /// Readable legacy columns only; current producer code leaves them
    /// NULL. Kept for decoding old snapshots, superseded by `workQuality`.
    public var correctness: Int?
    public var codeQuality: Int?
}

public struct SpecEpic: Codable, Equatable {
    public var id: String?
    public var title: String?
    public var status: String?
}

/// A spec-kind Athena consult with its minted work graph and dispatch states,
/// as served by alfonso-core's joined `athena.spec_status` projection.
/// `epic == nil` with empty slices means the consult is still in
/// clarify/rounds and the work graph has not been minted yet.
public struct SpecCampaign: Codable, Identifiable, Equatable {
    public var consultId: String
    public var phase: String?
    public var round: Int?
    public var updatedAtMs: Int64?
    public var draftPath: String?
    /// Attribution fields (additive; absent until alfonso-core serves them):
    /// the agent session that fired the campaign, for project/agent grouping.
    public var callerSessionId: String?
    public var callerHarness: String?
    public var displayName: String?
    public var epic: SpecEpic?
    public var slices: [SpecSlice]?

    public var id: String { consultId }
}
