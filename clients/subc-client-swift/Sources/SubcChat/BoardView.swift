import AppKit
import SwiftUI
import SubcChatAskSupport

/// Renders the session board as ordered lane sections. Ask actions deliberately go
/// through the shared AskViewModel so the Board tab cannot create a second answer path.
struct BoardView: View {
    @ObservedObject var vm: BoardViewModel
    @ObservedObject var asksVM: AskViewModel

    @State private var freeText: [String: String] = [:]

    var body: some View {
        VStack(spacing: 0) {
            header
            targetBar
            Divider()
            if vm.hasTarget && vm.opsAvailable == false {
                unavailable
            }
            boardContent
            if let health = vm.board?.health {
                Divider()
                healthStrip(health)
            }
        }
        .onAppear { vm.appear() }
        .onDisappear { vm.disappear() }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text("Board").font(.headline)
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
                ForEach(blocks) { block in
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
                Text(props.text)
                    .textSelection(.enabled)
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
            // Projected asks carry read-time age; board-minted asks from older
            // module builds don't, so the row simply doesn't render there.
            if let age = props.ageMs {
                Text("asked \(formatAge(age)) ago")
                    .font(.caption2)
                    .foregroundColor(.secondary)
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
            Text(props.body)
                .font(.system(size: 11, design: .monospaced))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func digestHeader(_ block: BoardBlock, suppressTitle: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            if !suppressTitle {
                Text(block.digest.title)
                    .font(.system(size: 13, weight: .bold))
                    .textSelection(.enabled)
            }
            if let line2 = block.digest.line2 {
                Text(line2)
                    .font(.caption)
                    .foregroundColor(.secondary)
                    .textSelection(.enabled)
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
