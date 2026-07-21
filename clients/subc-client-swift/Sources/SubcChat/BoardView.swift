import AppKit
import SwiftUI
import SubcChatAskSupport

/// Renders the session board as ordered lane sections. Ask actions deliberately go
/// through the shared AskViewModel so the Board tab cannot create a second answer path.
struct BoardView: View {
    @ObservedObject var vm: BoardViewModel
    @ObservedObject var asksVM: AskViewModel

    @State private var freeText: [String: String] = [:]
    /// Blocks whose full (untruncated) body the user asked for. Unbounded Text
    /// bodies with selection enabled make layout quadratic and freeze the window
    /// (same class as the transcript-sheet fast-scroll hang).
    @State private var expandedBlocks: Set<String> = []

    private static let blockCharBudget = 4_000
    /// Newest blocks rendered per lane before a show-earlier toggle.
    private static let laneBlockCap = 30
    @State private var expandedLanes: Set<String> = []

    var body: some View {
        VStack(spacing: 0) {
            header
            if vm.hasTarget {
                Divider()
                if vm.opsAvailable == false {
                    unavailable
                }
                boardContent
                if let health = vm.board?.health {
                    Divider()
                    healthStrip(health)
                }
            } else if let summaries = vm.summaries {
                Divider()
                pickerGrid(summaries)
            } else {
                // board.list unavailable (older alfonso-core): manual targeting.
                targetBar
                Divider()
                Spacer()
                Text("Enter an agent session id to view its board")
                    .font(.caption).foregroundColor(.secondary)
                Spacer()
            }
        }
        .onAppear { vm.appear() }
        .onDisappear { vm.disappear() }
    }

    // MARK: Picker grid (sessions with board data)

    private func pickerGrid(_ summaries: [BoardSummary]) -> some View {
        ScrollView {
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 260), spacing: 10)], spacing: 10) {
                ForEach(summaries) { s in
                    boardCard(s)
                        .contentShape(Rectangle())
                        .onTapGesture { vm.open(s) }
                }
            }
            .padding(10)
            if summaries.isEmpty {
                Text("No sessions have board data yet")
                    .font(.caption).foregroundColor(.secondary)
                    .padding(30)
            }
        }
    }

    private func boardCard(_ s: BoardSummary) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Circle()
                    .fill(cardStateColor(s.statusState))
                    .frame(width: 8, height: 8)
                Text(s.harness).font(.caption).foregroundColor(.secondary)
                Spacer()
                if let asks = s.openAsks, asks > 0 {
                    Text("\(asks) ask\(asks == 1 ? "" : "s")")
                        .font(.caption2).bold()
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(Capsule().fill(Color.orange.opacity(0.25)))
                }
            }
            Text(cardTitle(s))
                .font(.system(size: 12, weight: .semibold))
                .lineLimit(1)
            if let status = s.statusText, !status.isEmpty {
                Text(status)
                    .font(.system(size: 11))
                    .foregroundColor(.secondary)
                    .lineLimit(3)
            } else {
                Text("no status posted")
                    .font(.system(size: 11)).foregroundColor(.secondary).italic()
            }
            HStack(spacing: 8) {
                if let blocks = s.blockCount {
                    Text("\(blocks) block\(blocks == 1 ? "" : "s")").font(.caption2).foregroundColor(.secondary)
                }
                Spacer()
                if let ts = s.updatedAtMs {
                    Text(BoardView.relativeTime(ts)).font(.caption2).foregroundColor(.secondary)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity((s.blockCount ?? 0) == 0 ? 0.03 : 0.06)))
        .opacity((s.blockCount ?? 0) == 0 ? 0.6 : 1.0)
    }

    /// Card title: project folder name when known, else the session id tail.
    private func cardTitle(_ s: BoardSummary) -> String {
        if let root = s.projectRoot, !root.isEmpty {
            let name = (root as NSString).lastPathComponent
            if !name.isEmpty { return name }
        }
        return String(s.session.suffix(20))
    }

    private func cardStateColor(_ state: String?) -> Color {
        switch state {
        case "working": return .blue
        case "done": return .green
        case "blocked", "error": return .red
        default: return .gray
        }
    }

    static func relativeTime(_ epochMs: Int64) -> String {
        let delta = max(0, Date().timeIntervalSince1970 - Double(epochMs) / 1000)
        if delta < 90 { return "now" }
        if delta < 3600 { return "\(Int(delta / 60))m ago" }
        if delta < 86_400 { return "\(Int(delta / 3600))h ago" }
        return "\(Int(delta / 86_400))d ago"
    }

    private var header: some View {
        HStack(spacing: 8) {
            if vm.hasTarget, vm.summaries != nil {
                Button {
                    vm.closeBoard()
                } label: {
                    Label("Boards", systemImage: "chevron.left")
                }
                .buttonStyle(.plain)
                .font(.caption)
            }
            Text(vm.hasTarget ? "Board" : "Boards").font(.headline)
            if vm.hasTarget {
                Text(String(vm.targetSession.suffix(12))).font(.caption2).foregroundColor(.secondary)
            }
            if let board = vm.board {
                Text(board.vocabulary).font(.caption).foregroundColor(.secondary)
                Text("seq \(board.servedSeq)").font(.caption2).foregroundColor(.secondary)
            }
            Spacer()
            Circle()
                .fill(vm.opsAvailable == true ? Color.green : (vm.opsAvailable == false ? Color.orange : Color.gray))
                .frame(width: 8, height: 8)
            Text(vm.status).font(.caption).foregroundColor(.secondary)
        }
        .padding(10)
    }

    private var unavailable: some View {
        HStack(spacing: 6) {
            Image(systemName: "square.grid.2x2")
            Text("No board for this session (flag off, or session id mismatch)")
                .font(.caption)
        }
        .padding(8)
        .frame(maxWidth: .infinity)
        .background(Color.orange.opacity(0.12))
    }

    /// Manual board targeting until the module ships board.list discovery: the
    /// board is owned by an AGENT session, so the app must be pointed at one.
    private var targetBar: some View {
        HStack(spacing: 8) {
            TextField("harness", text: $vm.targetHarness)
                .textFieldStyle(.roundedBorder)
                .frame(width: 110)
            TextField("agent session id (owns the board)", text: $vm.targetSession)
                .textFieldStyle(.roundedBorder)
                .onSubmit { vm.applyTarget() }
            Button("Connect") { vm.applyTarget() }
                .disabled(vm.targetSession.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .padding(.horizontal, 10)
        .padding(.bottom, 8)
    }

    @ViewBuilder
    private var boardContent: some View {
        if let board = vm.board, !board.blocks.isEmpty {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 14) {
                    ForEach(board.lanes, id: \.self) { lane in
                        laneSection(lane, blocks: board.blocks.filter { $0.lane == lane })
                    }
                }
                .padding(12)
            }
        } else if !vm.hasTarget {
            VStack(spacing: 8) {
                Spacer()
                Image(systemName: "square.grid.2x2").font(.title2).foregroundColor(.secondary)
                Text("Enter the agent session id whose board to show")
                    .foregroundColor(.secondary)
                Spacer()
            }
        } else if vm.opsAvailable == false {
            Spacer()
        } else {
            VStack(spacing: 8) {
                Spacer()
                Text("Loading board…").foregroundColor(.secondary)
                Spacer()
            }
        }
    }

    private func laneSection(_ lane: String, blocks: [BoardBlock]) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(lane)
                .font(.headline)
                .frame(maxWidth: .infinity, alignment: .leading)
            if blocks.isEmpty {
                Text("No blocks")
                    .font(.caption)
                    .foregroundColor(.secondary)
                    .padding(.leading, 4)
            } else {
                // Live sessions grow the chat lane on every poll; an unbounded
                // ForEach makes each re-diff proportional to session length.
                let visible = expandedLanes.contains(lane) ? blocks : Array(blocks.suffix(Self.laneBlockCap))
                if blocks.count > visible.count {
                    Button("Show \(blocks.count - visible.count) earlier") {
                        expandedLanes.insert(lane)
                    }
                    .buttonStyle(.link)
                    .font(.caption)
                }
                ForEach(visible) { block in
                    blockView(block)
                }
            }
        }
    }

    @ViewBuilder
    private func blockView(_ block: BoardBlock) -> some View {
        switch block.props {
        case let .ask(props) where block.kind == "ask":
            askCard(block, props: props)
        case let .show(props) where block.kind == "show":
            showCard(block, props: props)
        case let .text(props) where block.kind == "text":
            textCard(block, props: props)
        default:
            digestCard(block)
        }
    }

    private func digestCard(_ block: BoardBlock) -> some View {
        cardContainer(block) {
            digestHeader(block)
        }
    }

    private func textCard(_ block: BoardBlock, props: BoardTextProps) -> some View {
        cardContainer(block) {
            digestHeader(block, suppressTitle: block.digest.title == props.text)
            HStack(alignment: .firstTextBaseline, spacing: 5) {
                if props.producer == "tee" {
                    Text("⌁")
                        .foregroundColor(.secondary)
                        .help("tee-produced")
                }
                boundedBody(props.text, blockId: block.blockId)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            if let defect = props.teeDefect {
                HStack(spacing: 4) {
                    badgeChip("partial", color: .yellow)
                    Text("tee: \(defect)")
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    private func askCard(_ block: BoardBlock, props: BoardAskProps) -> some View {
        cardContainer(block) {
            digestHeader(block)
            if props.status.lowercased() == "answered" {
                if let answer = props.answer {
                    Text("Answer: \(answer)")
                        .textSelection(.enabled)
                }
                Text("answered")
                    .font(.caption)
                    .foregroundColor(.secondary)
            } else if props.status.lowercased() == "pending" {
                Text("Pending answer")
                    .font(.caption)
                    .foregroundColor(.secondary)
                if let options = props.options {
                    ForEach(options, id: \.self) { option in
                        Button {
                            asksVM.persistAnswer(option, requestID: props.requestId)
                        } label: {
                            Text(option)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .buttonStyle(.bordered)
                        .disabled(!asksVM.canAnswer(requestID: props.requestId))
                    }
                }
                if props.allowFreeText == true {
                    HStack(spacing: 8) {
                        TextField("Your answer", text: Binding(
                            get: { freeText[props.requestId] ?? "" },
                            set: { freeText[props.requestId] = $0 }))
                            .textFieldStyle(.roundedBorder)
                        Button("Send") {
                            asksVM.persistAnswer(freeText[props.requestId] ?? "", requestID: props.requestId)
                            freeText[props.requestId] = ""
                        }
                        .disabled(!asksVM.canAnswer(requestID: props.requestId)
                            || (freeText[props.requestId] ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
            } else {
                Text(props.status)
                    .font(.caption)
                    .foregroundColor(.secondary)
                if let answer = props.answer {
                    Text("Answer: \(answer)").textSelection(.enabled)
                }
            }
            if let policy = props.silencePolicy, let waitUntil = policy.waitUntil,
               let countdown = countdown(for: waitUntil) {
                countdownChip(countdown)
            }
            // Age computed locally from the stable askedAt timestamp: the
            // wire's ageMs is stripped at ingest because a per-poll-changing
            // field defeats change-detection and forces full re-diffs.
            if let askedAt = props.askedAt {
                let age = Int64(Date().timeIntervalSince1970 * 1_000) - askedAt
                if age > 0 {
                    Text("asked \(formatAge(age)) ago")
                        .font(.caption2)
                        .foregroundColor(.secondary)
                }
            }
        }
    }

    private func formatAge(_ ms: Int64) -> String {
        let s = ms / 1_000
        if s < 60 { return "\(s)s" }
        if s < 3_600 { return "\(s / 60)m" }
        if s < 86_400 { return "\(s / 3_600)h \((s % 3_600) / 60)m" }
        return "\(s / 86_400)d \((s % 86_400) / 3_600)h"
    }

    private func showCard(_ block: BoardBlock, props: BoardShowProps) -> some View {
        cardContainer(block) {
            digestHeader(block)
            HStack(spacing: 6) {
                if let language = props.language {
                    Text(language)
                        .font(.system(size: 10, weight: .medium, design: .monospaced))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 2)
                        .background(Color.gray.opacity(0.14))
                        .clipShape(Capsule())
                }
                Spacer()
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(props.body, forType: .string)
                } label: {
                    Label("Copy", systemImage: "doc.on.doc")
                }
                .buttonStyle(.borderless)
                .font(.caption)
            }
            boundedBody(props.body, blockId: block.blockId)
                .font(.system(size: 11, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// Renders a body string truncated to the char budget with a show-all
    /// toggle. Selection stays enabled on the bounded prefix only.
    @ViewBuilder
    private func boundedBody(_ full: String, blockId: String) -> some View {
        if full.count <= Self.blockCharBudget || expandedBlocks.contains(blockId) {
            Text(full).textSelection(.enabled)
        } else {
            VStack(alignment: .leading, spacing: 4) {
                Text(String(full.prefix(Self.blockCharBudget)) + " …")
                    .textSelection(.enabled)
                Button("Show all (\(full.count) chars)") {
                    expandedBlocks.insert(blockId)
                }
                .buttonStyle(.link)
                .font(.caption)
            }
        }
    }

    private func digestHeader(_ block: BoardBlock, suppressTitle: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            // No text selection on headers: every selection-enabled Text adds
            // a SelectionOverlay whose update cost is paid on EVERY re-diff —
            // with hundreds of blocks this alone wedged the main thread
            // (watchdog samples, 2026-07-19). Bodies keep selection; titles
            // are digest summaries with copyable bodies below them.
            if !suppressTitle {
                Text(block.digest.title)
                    .font(.system(size: 13, weight: .bold))
            }
            if let line2 = block.digest.line2 {
                Text(line2)
                    .font(.caption)
                    .foregroundColor(.secondary)
            }
            HStack(spacing: 5) {
                if let badge = block.digest.badge {
                    badgeChip(badge, color: badgeColor(badge))
                }
                if let urgency = block.digest.urgency {
                    Text(urgency)
                        .font(.system(size: 9, weight: .medium))
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .foregroundColor(urgencyColor(urgency))
                        .background(urgencyColor(urgency).opacity(0.18))
                        .clipShape(Capsule())
                }
            }
        }
    }

    private func cardContainer<Content: View>(
        _ block: BoardBlock,
        @ViewBuilder content: () -> Content
    ) -> some View {
        HStack(spacing: 0) {
            if let urgency = block.digest.urgency {
                Rectangle()
                    .fill(urgencyColor(urgency))
                    .frame(width: 3)
            }
            VStack(alignment: .leading, spacing: 7, content: content)
                .padding(9)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Color.gray.opacity(0.09))
        .cornerRadius(7)
    }

    private func badgeChip(_ text: String, color: Color) -> some View {
        Text(text)
            .font(.system(size: 9, weight: .bold))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .foregroundColor(color == .yellow ? .orange : color)
            .background(color.opacity(0.22))
            .clipShape(Capsule())
    }

    private func countdownChip(_ text: String) -> some View {
        Text(text)
            .font(.system(size: 9, weight: .medium))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .foregroundColor(.orange)
            .background(Color.orange.opacity(0.14))
            .clipShape(Capsule())
    }

    private func badgeColor(_ badge: String) -> Color {
        switch badge.lowercased() {
        case "working": return .blue
        case "ask": return .orange
        case "answered": return .green
        case "auto_proceeded": return .purple
        case "partial": return .yellow
        case "opaque": return .gray
        default: return .gray
        }
    }

    private func urgencyColor(_ urgency: String) -> Color {
        switch urgency.lowercased() {
        case "high", "critical": return .red
        case "low": return .secondary
        default: return .orange
        }
    }

    private func countdown(for waitUntil: Int64) -> String? {
        let seconds = Int(Date(timeIntervalSince1970: TimeInterval(waitUntil) / 1_000).timeIntervalSinceNow)
        guard seconds > 0 else { return nil }
        if seconds < 60 { return "auto-proceeds in \(seconds)s" }
        if seconds < 3_600 { return "auto-proceeds in \(Int(ceil(Double(seconds) / 60)))m" }
        return "auto-proceeds in \(Int(ceil(Double(seconds) / 3_600)))h"
    }

    private func healthStrip(_ health: BoardHealth) -> some View {
        HStack(spacing: 9) {
            Image(systemName: "heart.text.square")
            if let tee = health.props.teeCounters {
                counter("tee ok", tee.wellFormed)
                counter("tee partial", tee.malformed)
                counter("leaked", tee.leakedOtherHarness)
                counter("mimicry", tee.syntaxMimicry)
            }
            if let rung2 = health.props.rung2Counters {
                counter("rung2 prose", rung2.proseQuestionsAtTurnEnd)
            }
            if let rung3 = health.props.rung3Counters {
                counter("rung3 nudges", rung3.nudges)
                if let stale = rung3.staleChipShown {
                    Text("stale \(stale ? "yes" : "no")")
                        .font(.system(size: 10, design: .monospaced))
                }
            }
            Spacer()
        }
        .font(.system(size: 10))
        .foregroundColor(.secondary)
        .opacity(0.75)
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    @ViewBuilder
    private func counter(_ label: String, _ value: Int?) -> some View {
        if let value {
            Text("\(label) \(value)")
                .font(.system(size: 10, design: .monospaced))
        }
    }
}
