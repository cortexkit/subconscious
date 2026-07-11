import SwiftUI

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
        case .athena: athenaSplit
        case .gather: runList(vm.gathers)
        case .checks: runList(vm.checks)
        }
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
                        VStack(alignment: .leading, spacing: 2) {
                            Text("\(msg.role) · #\(msg.ordinal)")
                                .font(.caption2).foregroundColor(.secondary)
                            if !msg.text.isEmpty {
                                Text(msg.text)
                                    .font(.system(size: 12))
                                    .textSelection(.enabled)
                                    .padding(8)
                                    .background(msg.role == "user"
                                        ? Color.accentColor.opacity(0.12)
                                        : Color.gray.opacity(0.10))
                                    .cornerRadius(8)
                            }
                            ForEach(msg.blockSummaries, id: \.self) { s in
                                Text(s).font(.system(size: 11)).foregroundColor(.secondary)
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
}
