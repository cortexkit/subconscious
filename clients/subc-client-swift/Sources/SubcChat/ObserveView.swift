import SwiftUI
import SubcChatAskSupport

/// The three alfonso observability lanes: Athena consults, gather_context runs,
/// and comment-check oneshots. One shared view model; the tab picker selects the
/// lane. Every lane can open the shared broca transcript sheet for any session id
/// it surfaces.
struct ObserveView: View {
    @ObservedObject var vm: ObserveViewModel
    let lane: Lane

    enum Lane {
        case athena, gather, checks
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if vm.opsAvailable == false {
                banner
            }
            content
        }
        .onAppear { vm.appear() }
        .onDisappear { vm.disappear() }
        .sheet(isPresented: Binding(
            get: { vm.transcriptFor != nil },
            set: { if !$0 { vm.closeTranscript() } })) {
            TranscriptSheet(vm: vm)
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text(title).font(.headline)
            Spacer()
            Circle()
                .fill(vm.status == "live" ? Color.green : (vm.opsAvailable == false ? Color.orange : Color.gray))
                .frame(width: 8, height: 8)
            Text(vm.status).font(.caption).foregroundColor(.secondary)
        }
        .padding(10)
    }

    private var title: String {
        switch lane {
        case .athena: return "Athena Consults"
        case .gather: return "Context Gathers"
        case .checks: return "Comment Checks"
        }
    }

    private var banner: some View {
        HStack(spacing: 6) {
            Image(systemName: "hourglass")
            Text("alfonso-core's observability ops haven't deployed yet — this tab lights up when they land.")
                .font(.caption)
        }
        .padding(8)
        .frame(maxWidth: .infinity)
        .background(Color.orange.opacity(0.12))
    }

    @ViewBuilder
    private var content: some View {
        switch lane {
        case .athena:
            VStack(spacing: 0) {
                if !vm.specCampaigns.isEmpty {
                    specCampaignSection
                    Divider()
                }
                athenaSplit
                    // Fill-and-win: without an explicit flexible frame plus
                    // layout priority, the enclosing VStack asks this
                    // platform-backed split (NSTableView List) for its IDEAL
                    // height to divide space — which walks Auto Layout
                    // constraints for every row subtree and froze the main
                    // thread for seconds (stall-2026-07-21T17-12-10Z).
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .layoutPriority(1)
            }
        case .gather: runList(vm.gathers)
        case .checks: runList(vm.checks)
        }
    }

    // MARK: Spec campaigns (draft -> rounds -> minted graph -> dispatched slices)

    private var specCampaignSection: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 6) {
                Text("Spec Campaigns").font(.caption).bold().foregroundColor(.secondary)
                ForEach(vm.specCampaigns) { campaign in
                    specCampaignCard(campaign)
                }
            }
            .padding(8)
        }
        // Fixed height, deliberately not maxHeight: clamping to a maximum
        // requires measuring the NSScrollView's ideal content height, which
        // re-enters the same whole-subtree constraint walk as the List below.
        .frame(height: 240)
    }

    private func specCampaignCard(_ c: SpecCampaign) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                stateChip(c.phase ?? "?")
                if let r = c.round { Text("round \(r)").font(.caption2).foregroundColor(.secondary) }
                Text(c.epic?.title ?? draftName(c.draftPath) ?? c.consultId)
                    .font(.system(size: 12, weight: .semibold))
                    .lineLimit(1)
                Spacer()
                if let ts = c.updatedAtMs {
                    Text(Self.timeString(ts)).font(.caption2).foregroundColor(.secondary)
                }
            }
            if let slices = c.slices, !slices.isEmpty {
                // Ladder order is dispatch order: the pipeline reads top-to-bottom.
                ForEach(slices) { slice in
                    specSliceRow(slice)
                }
            } else {
                Text("work graph not minted yet")
                    .font(.caption2).foregroundColor(.secondary).italic()
                    .padding(.leading, 12)
            }
        }
        .padding(6)
        .background(RoundedRectangle(cornerRadius: 6).fill(Color.primary.opacity(0.04)))
    }

    private func specSliceRow(_ s: SpecSlice) -> some View {
        HStack(spacing: 6) {
            stateChip(s.status ?? "?")
            Text(s.title ?? s.id).font(.system(size: 11)).lineLimit(1)
            if let v = s.verifyLeaf?.status, v != "open" {
                Text("verify: \(v)").font(.caption2).foregroundColor(.secondary)
            }
            Spacer()
            if let d = s.dispatch {
                if let scores = d.scores, let corr = scores.correctness, let q = scores.codeQuality {
                    Text("\(corr)/\(q)").font(.caption2).foregroundColor(.secondary)
                        .help("correctness \(corr) · code quality \(q)")
                }
                if let state = d.taskState {
                    stateChip(state)
                }
                if let reason = d.failureReason {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.caption2).foregroundColor(.red)
                        .help(reason)
                }
            } else {
                Text("queued").font(.caption2).foregroundColor(.secondary)
            }
        }
        .padding(.leading, 12)
    }

    private func draftName(_ path: String?) -> String? {
        guard let path else { return nil }
        let base = (path as NSString).lastPathComponent
        return base.isEmpty ? nil : base
    }

    // MARK: Athena lane (list + detail split)

    private var athenaSplit: some View {
        HSplitView {
            List(vm.consults, selection: Binding(
                get: { vm.selectedConsultId },
                set: { if let id = $0 { vm.selectConsult(id) } })) { row in
                consultRow(row).tag(row.consultId)
            }
            .frame(minWidth: 320)
            consultDetailPane
                .frame(minWidth: 300, maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func consultRow(_ row: ConsultRow) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                stateChip(row.terminalReason ?? row.phase ?? "?")
                Text(row.consultClass ?? "").font(.caption2).foregroundColor(.secondary)
                if let s = row.sentinels, !s.isEmpty {
                    Image(systemName: "flag.fill").font(.caption2).foregroundColor(.orange)
                        .help(s.joined(separator: ", "))
                }
                Spacer()
                if let ts = row.startedAtMs {
                    Text(Self.timeString(ts)).font(.caption2).foregroundColor(.secondary)
                }
            }
            Text(row.questionPreview ?? row.consultId)
                .font(.system(size: 12))
                .lineLimit(2)
        }
        .padding(.vertical, 3)
    }

    private var consultDetailPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 10) {
                if let d = vm.consultDetail {
                    if let q = d.questionPreview {
                        section("Question", q)
                    }
                    if let cp = d.currentPhase {
                        section("Phase", "\(cp.phase ?? "?")" + (cp.round.map { $0 >= 0 ? " · round \($0)" : "" } ?? ""))
                    }
                    if let s = d.sentinels, !s.isEmpty {
                        section("Sentinels", s.joined(separator: ", "))
                    }
                    if let attempts = d.attempts, !attempts.isEmpty {
                        // Panel members are the fanout/merge attempts; classify and
                        // gather attempts are pipeline stages, not members (both often
                        // route to the same model, so unsplit they read as duplicate
                        // "members"). Split by phase, keeping both visible.
                        let members = attempts.filter { ["fanout", "merge"].contains($0.phase ?? "") }
                        let pipeline = attempts.filter { !["fanout", "merge"].contains($0.phase ?? "") }
                        if !pipeline.isEmpty {
                            Text("Pipeline").font(.caption).bold()
                            ForEach(pipeline) { a in attemptRow(a, showPhase: true) }
                        }
                        if !members.isEmpty {
                            Text("Members").font(.caption).bold()
                            ForEach(members) { a in attemptRow(a, showPhase: false) }
                        }
                    }
                    if let ev = d.evidence, let c = ev.count, c > 0 {
                        section("Evidence", "\(c) unit(s)" + (ev.unitKinds.map { " — " + $0.map { "\($0.key): \($0.value)" }.sorted().joined(separator: ", ") } ?? ""))
                    }
                    if let s = d.synthesis, s.present == true, let r = s.resultPreview, !r.isEmpty {
                        section("Synthesis\(s.mechanical == true ? " (mechanical)" : "")", r)
                    }
                    if let tu = d.tokenUsage {
                        tokenUsageSection(tu)
                    }
                } else if vm.selectedConsultId != nil {
                    ProgressView().padding(30)
                } else {
                    Text("Select a consult").foregroundColor(.secondary).padding(30)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    // MARK: Gather / Checks lanes (flat run lists)

    private func runList(_ runs: [ObservedRun]) -> some View {
        List(runs) { run in
            HStack(spacing: 8) {
                stateChip(run.state ?? "?")
                VStack(alignment: .leading, spacing: 2) {
                    Text(run.preview ?? run.sessionId ?? "run")
                        .font(.system(size: 12))
                        .lineLimit(2)
                    HStack(spacing: 6) {
                        if let m = run.model {
                            Text(m).font(.caption2).foregroundColor(.secondary)
                        }
                        if let ts = run.startedAtMs {
                            Text(Self.timeString(ts)).font(.caption2).foregroundColor(.secondary)
                        }
                    }
                }
                Spacer()
                if let sid = run.sessionId {
                    Button("transcript") {
                        vm.openTranscript(sessionId: sid, projectRoot: run.projectRoot)
                    }
                    .font(.caption2)
                }
            }
            .padding(.vertical, 2)
        }
    }

    // MARK: Shared bits

    private func attemptRow(_ a: ConsultAttempt, showPhase: Bool) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 6) {
                stateChip(a.state ?? "?")
                if showPhase, let p = a.phase {
                    Text(p).font(.caption2).foregroundColor(.secondary)
                }
                Text(a.model?.label ?? a.subjectKey ?? "member").font(.system(size: 12))
                Spacer()
                if let sid = a.sessionId {
                    Button("transcript") {
                        vm.openTranscript(sessionId: sid, projectRoot: a.projectRoot)
                    }
                    .font(.caption2)
                }
            }
            usageLine(a.usage)
        }
    }

    // Per-run usage under each attempt row. Providers split prompt tokens
    // across input/cachedInput/cacheWrite differently (anthropic cache-warmed
    // sends report input=1 with the rest under cacheWrite; kimi bills the
    // whole prompt as cachedInput with input=0), so the headline is the SUM
    // of the three, with the split and output/reasoning shown beside it.
    @ViewBuilder
    private func usageLine(_ u: AttemptUsage?) -> some View {
        if let u = u {
            let prompt = (u.inputTokens ?? 0) + (u.cachedInputTokens ?? 0) + (u.cacheWriteTokens ?? 0)
            HStack(spacing: 8) {
                tokenChip("prompt", prompt)
                if (u.cachedInputTokens ?? 0) > 0 { tokenChip("cached", u.cachedInputTokens!) }
                if (u.cacheWriteTokens ?? 0) > 0 { tokenChip("cacheW", u.cacheWriteTokens!) }
                tokenChip("out", u.outputTokens ?? 0)
                if (u.reasoningTokens ?? 0) > 0 { tokenChip("reason", u.reasoningTokens!) }
                if let r = u.retriesUsed, r > 0 {
                    Text("\(r) retr").font(.system(size: 10)).foregroundColor(.orange)
                }
            }
            .padding(.leading, 46)
        } else {
            Text("unmeasured")
                .font(.system(size: 10)).foregroundColor(.secondary.opacity(0.6))
                .padding(.leading, 46)
        }
    }

    private func tokenChip(_ label: String, _ value: Int64) -> some View {
        HStack(spacing: 2) {
            Text(label).foregroundColor(.secondary)
            Text(TokenFormat.count(value)).monospacedDigit()
        }
        .font(.system(size: 10))
    }



    // Server-computed rollup: the total row also counts unmeasured attempts,
    // which a client-side sum over present usage objects would silently miss.
    @ViewBuilder
    private func tokenUsageSection(_ tu: TokenUsageRollup) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Token Usage").font(.caption).bold()
            if let rows = tu.models {
                ForEach(Array(rows.enumerated()), id: \.offset) { _, m in
                    tokenUsageRow(m, bold: false)
                }
            }
            if let t = tu.total {
                Divider().padding(.vertical, 1)
                tokenUsageRow(t, bold: true)
            }
        }
    }

    private func tokenUsageRow(_ m: TokenUsageModelRow, bold: Bool) -> some View {
        HStack(spacing: 8) {
            Text(bold ? "total" : (m.model ?? "?"))
                .font(.system(size: 11, weight: bold ? .semibold : .regular))
                .frame(minWidth: 150, alignment: .leading)
            let prompt = (m.input ?? 0) + (m.cachedInput ?? 0) + (m.cacheWrite ?? 0)
            tokenChip("prompt", prompt)
            if (m.cachedInput ?? 0) > 0 { tokenChip("cached", m.cachedInput!) }
            if (m.cacheWrite ?? 0) > 0 { tokenChip("cacheW", m.cacheWrite!) }
            tokenChip("out", m.output ?? 0)
            if (m.reasoning ?? 0) > 0 { tokenChip("reason", m.reasoning!) }
            if let c = m.calls { Text("\(c) call\(c == 1 ? "" : "s")").font(.system(size: 10)).foregroundColor(.secondary) }
            if let un = m.unmeasured, un > 0 {
                Text("\(un) unmeasured").font(.system(size: 10)).foregroundColor(.orange)
            }
            Spacer()
        }
    }

    private func section(_ label: String, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label).font(.caption).bold()
            Text(text)
                .font(.system(size: 12))
                .textSelection(.enabled)
        }
    }

    private func stateChip(_ state: String) -> some View {
        Text(state)
            .font(.caption2)
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(chipColor(state).opacity(0.18))
            .foregroundColor(chipColor(state))
            .cornerRadius(4)
    }

    private func chipColor(_ state: String) -> Color {
        switch state.lowercased() {
        case "completed", "done", "ok", "passed": return .green
        case "error", "failed": return .red
        case "active", "running", "gathering", "synthesizing": return .orange
        case "paused": return .yellow
        default: return .gray
        }
    }

    static func timeString(_ epochMs: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(epochMs) / 1000)
        let fmt = DateFormatter()
        fmt.dateFormat = "HH:mm:ss"
        return fmt.string(from: date)
    }
}

// MARK: - Shared transcript sheet (broca session.read; works independently of ALF's ops)

struct TranscriptSheet: View {
    @ObservedObject var vm: ObserveViewModel
    /// System rows render collapsed (one-line preview) so long instruction
    /// blocks don't bury the conversation; tapping a row toggles the full text.
    @State private var expandedSystemRows: Set<Int64> = []
    /// Rows whose full (untruncated) body the user asked for. Everything else
    /// renders a bounded prefix: unbounded Text bodies with selection enabled
    /// make lazy-scroll layout quadratic and freeze the window on fast scroll.
    @State private var expandedFullRows: Set<Int64> = []

    /// Beyond this many characters a row renders truncated with a show-all
    /// toggle. Big enough that normal chat turns never truncate.
    private static let rowCharBudget = 4_000

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Session transcript").font(.headline)
                    if let sid = vm.transcriptFor {
                        Text(sid).font(.caption2).foregroundColor(.secondary).textSelection(.enabled)
                    }
                }
                Spacer()
                if let ls = vm.transcriptLineage {
                    HStack(spacing: 6) {
                        Text(ls.state ?? "").font(.caption)
                        if let r = ls.reason { Text(r).font(.caption2).foregroundColor(.secondary) }
                    }
                }
                Text(vm.transcriptStatus).font(.caption2).foregroundColor(.secondary)
                Button("Close") { vm.closeTranscript() }
            }
            .padding(10)
            Divider()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 8) {
                    ForEach(vm.transcript) { msg in
                        if msg.role == "system" {
                            systemRow(msg)
                            } else {
                                VStack(alignment: .leading, spacing: 2) {
                                    Text("\(msg.role) · #\(msg.ordinal)")
                                        .font(.caption2).foregroundColor(.secondary)
                                    if !msg.text.isEmpty {
                                        boundedBody(msg)
                                            .padding(8)
                                            .background(msg.role == "user"
                                                ? Color.accentColor.opacity(0.12)
                                                : Color.gray.opacity(0.10))
                                            .cornerRadius(8)
                                    }
                                    // Index-keyed: summary strings can repeat (two identical
                                    // tool results), and duplicate ForEach ids corrupt diffing.
                                    ForEach(Array(msg.blockSummaries.enumerated()), id: \.offset) { _, s in
                                        Text(s).font(.system(size: 11)).foregroundColor(.secondary)
                                    }
                                }
                            }
                    }
                    if let err = vm.transcriptLineage?.errorText, !err.isEmpty {
                        Text(err)
                            .font(.system(size: 11))
                            .foregroundColor(.red)
                            .padding(8)
                            .background(Color.red.opacity(0.10))
                            .cornerRadius(8)
                    }
                }
                .padding(12)
            }
        }
        .frame(minWidth: 640, minHeight: 480)
    }

    /// Bounded row body: renders at most `rowCharBudget` characters unless the
    /// user expands the row. Selection stays enabled; only the byte volume fed
    /// to a single Text layout pass is capped.
    @ViewBuilder
    private func boundedBody(_ msg: TranscriptMessage) -> some View {
        let full = msg.text
        let over = full.count > Self.rowCharBudget && !expandedFullRows.contains(msg.ordinal)
        VStack(alignment: .leading, spacing: 4) {
            Text(over ? String(full.prefix(Self.rowCharBudget)) : full)
                .font(.system(size: 12))
                .textSelection(.enabled)
            if over {
                Button("Show all (\(full.count) chars)") {
                    expandedFullRows.insert(msg.ordinal)
                }
                .font(.caption2)
                .buttonStyle(.link)
            }
        }
    }

    /// Collapsed-by-default system prompt row: header + first line preview,
    /// tap to expand to the full selectable text.
    @ViewBuilder
    private func systemRow(_ msg: TranscriptMessage) -> some View {
        let expanded = expandedSystemRows.contains(msg.ordinal)
        VStack(alignment: .leading, spacing: 2) {
            HStack(spacing: 4) {
                Image(systemName: expanded ? "chevron.down" : "chevron.right")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundColor(.secondary)
                Text("system · #\(msg.ordinal)")
                    .font(.caption2).foregroundColor(.secondary)
                if !expanded {
                    Text(msg.text.split(separator: "\n").first.map(String.init) ?? "")
                        .font(.system(size: 11))
                        .foregroundColor(.secondary)
                        .lineLimit(1)
                }
            }
            .contentShape(Rectangle())
            .onTapGesture {
                if expanded { expandedSystemRows.remove(msg.ordinal) } else { expandedSystemRows.insert(msg.ordinal) }
            }
                    if expanded {
                        boundedBody(msg)
                            .padding(8)
                            .background(Color.purple.opacity(0.08))
                            .cornerRadius(8)
                        ForEach(Array(msg.blockSummaries.enumerated()), id: \.offset) { _, s in
                            Text(s).font(.system(size: 11)).foregroundColor(.secondary)
                        }
                    }
        }
    }
}
