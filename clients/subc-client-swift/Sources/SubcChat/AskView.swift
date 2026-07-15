import Foundation
import SwiftUI
import SubcChatAskSupport

/// A human-facing list, detail, and action view for pending alfonso asks.
struct AskView: View {
    @ObservedObject var vm: AskViewModel

    @State private var answerText = ""
    @State private var amendText = ""
    @State private var resolutionText = ""

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if vm.opsAvailable == false {
                unavailableBanner
            }
            if vm.hasTransientError {
                errorBanner
            }
            HSplitView {
                askList
                detailPane
                    .frame(minWidth: 360, maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .onAppear { vm.appear() }
        .onDisappear { vm.disappear() }
        .onChange(of: vm.selectedAskId) { _ in
            answerText = ""
            amendText = ""
            resolutionText = ""
        }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text("Pending asks").font(.headline)
            Spacer()
            Circle()
                .fill(vm.status == "live" ? Color.green : (vm.opsAvailable == false ? Color.orange : Color.gray))
                .frame(width: 8, height: 8)
            Text(vm.status).font(.caption).foregroundColor(.secondary)
        }
        .padding(10)
    }

    private var unavailableBanner: some View {
        HStack(spacing: 6) {
            Image(systemName: "hourglass")
            Text("alfonso-core's ask ops haven't deployed yet — this tab lights up when they land.")
                .font(.caption)
        }
        .padding(8)
        .frame(maxWidth: .infinity)
        .background(Color.orange.opacity(0.12))
    }

    private var errorBanner: some View {
        HStack(spacing: 6) {
            Image(systemName: "exclamationmark.triangle.fill")
            Text(vm.status).font(.caption)
            Spacer()
        }
        .padding(8)
        .foregroundColor(.red)
        .background(Color.red.opacity(0.10))
    }

    // MARK: List

    private var askList: some View {
        List(selection: Binding(
            get: { vm.selectedAskId },
            set: { if let id = $0 { vm.selectAsk(id) } })) {
            if vm.asks.isEmpty, vm.opsAvailable != false {
                Text("No pending asks")
                    .foregroundColor(.secondary)
                    .frame(maxWidth: .infinity, alignment: .center)
                    .padding(.vertical, 24)
            }
            ForEach(vm.asks) { ask in
                askRow(ask).tag(ask.requestID)
            }
        }
        .frame(minWidth: 330)
    }

    private func askRow(_ ask: AskRequest) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 5) {
                urgencyChip(ask.urgency)
                if ask.materialDamage == true {
                    materialDamageChip
                }
                Spacer(minLength: 4)
                if let countdown = vetoCountdown(for: ask) {
                    countdownChip(countdown)
                }
            }
            Text(ask.question)
                .font(.system(size: 12))
                .lineLimit(2)
            HStack(spacing: 5) {
                Text(shortSession(ask.askerSessionID))
                    .font(.system(size: 10, design: .monospaced))
                Text(relativeTime(ask.askedAt))
                    .font(.caption2)
                Spacer()
            }
            .foregroundColor(.secondary)
        }
        .padding(.vertical, 3)
    }

    private func urgencyChip(_ urgency: String?) -> some View {
        let value = urgency ?? "normal"
        let color: Color
        switch value.lowercased() {
        case "high": color = .red
        case "low": color = .secondary
        default: color = .gray
        }
        return Text(value)
            .font(.system(size: 9, weight: .medium))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .foregroundColor(color)
            .background(color.opacity(0.18))
            .clipShape(Capsule())
    }

    private var materialDamageChip: some View {
        Text("material")
            .font(.system(size: 9, weight: .bold))
            .padding(.horizontal, 5)
            .padding(.vertical, 1)
            .foregroundColor(.orange)
            .background(Color.orange.opacity(0.24))
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

    // MARK: Detail

    private var detailPane: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                if let ask = vm.askDetail {
                    askDetail(ask)
                } else if vm.selectedAskId != nil {
                    ProgressView().padding(30)
                } else {
                    Text("Select an ask").foregroundColor(.secondary).padding(30)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder
    private func askDetail(_ ask: AskRequest) -> some View {
        Text(ask.question)
            .font(.title3.weight(.semibold))
            .textSelection(.enabled)

        if let notice = vm.actionNotice {
            Text(notice)
                .font(.caption)
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.accentColor.opacity(0.12))
                .cornerRadius(6)
        }

        if let state = ask.state {
            infoSection("State", state)
        }
        if let answer = ask.answer, !answer.isEmpty {
            infoSection("Recorded answer", answer)
        }
        if let resolution = ask.resolution, !resolution.isEmpty {
            infoSection("Resolution", resolution)
        }
        if let context = ask.context, !context.isEmpty {
            infoSection("Context", context)
        }
        if let whyItMatters = ask.whyItMatters, !whyItMatters.isEmpty {
            infoSection("Why it matters", whyItMatters)
        }
        if let scope = ask.scope, !scope.isEmpty {
            infoSection("Scope", scope)
        }
        if let refs = ask.refs, !refs.isEmpty {
            refsSection(refs)
        }
        if let reversibility = ask.reversibility {
            reversibilitySection(reversibility)
        }

        detailsSection(ask)

        if let decision = ask.defaultDecision, !decision.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                Text("If unanswered: \(decision)")
                    .font(.system(size: 12))
                    .textSelection(.enabled)
                if let deadline = vetoDeadlineText(for: ask) {
                    countdownChip(deadline)
                }
            }
        }

        answerControls(for: ask)
    }

    @ViewBuilder
    private func detailsSection(_ ask: AskRequest) -> some View {
        if ask.blocking != nil || ask.taskID != nil || ask.askerSessionID != nil || ask.purpose != nil || ask.recipientKind != nil {
            VStack(alignment: .leading, spacing: 4) {
                if let blocking = ask.blocking {
                    detailLine("Blocking", blocking ? "Yes" : "No")
                }
                if let taskID = ask.taskID {
                    detailLine("Task", taskID, monospaced: true)
                }
                if let askerSessionID = ask.askerSessionID {
                    detailLine("Asker", askerSessionID, monospaced: true)
                }
                if let purpose = ask.purpose {
                    detailLine("Purpose", purpose)
                }
                if let recipientKind = ask.recipientKind {
                    detailLine("Recipient", recipientKind)
                }
                detailLine("Asked", "\(absoluteTime(ask.askedAt)) · \(relativeTime(ask.askedAt))")
            }
            .padding(8)
            .background(Color.gray.opacity(0.08))
            .cornerRadius(6)
        } else {
            detailLine("Asked", "\(absoluteTime(ask.askedAt)) · \(relativeTime(ask.askedAt))")
        }
    }

    @ViewBuilder
    private func answerControls(for ask: AskRequest) -> some View {
        if vm.canAct(on: ask) {
            Divider().padding(.top, 2)
            VStack(alignment: .leading, spacing: 8) {
                Text("Answer").font(.headline)
                if ask.purpose == "campaign_approval" {
                    campaignApprovalControls
                } else {
                    optionControls(ask.options ?? [])
                    freeTextControls
                }
                dismissalControls(for: ask)
            }
        } else if !ask.isPending {
            Text("This ask is no longer pending.")
                .font(.caption)
                .foregroundColor(.secondary)
        }
    }

    @ViewBuilder
    private func optionControls(_ options: [AskOption]) -> some View {
        if !options.isEmpty {
            ForEach(options) { option in
                Button {
                    // The option label is the protocol answer, so do not rewrite it.
                    vm.persistAnswer(option.label)
                } label: {
                    VStack(alignment: .leading, spacing: 3) {
                        HStack {
                            Text(option.label).font(.system(size: 12, weight: .semibold))
                            if option.recommended == true {
                                Text("recommended")
                                    .font(.system(size: 9, weight: .bold))
                                    .foregroundColor(.accentColor)
                            }
                            Spacer()
                        }
                        if let description = option.description, !description.isEmpty {
                            Text(description).font(.caption).foregroundColor(.secondary)
                        }
                        if let tradeoff = option.tradeoff, !tradeoff.isEmpty {
                            Text("Tradeoff: \(tradeoff)").font(.caption2).foregroundColor(.secondary)
                        }
                    }
                    .padding(8)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(option.recommended == true ? Color.accentColor.opacity(0.12) : Color.gray.opacity(0.08))
                    .overlay(
                        RoundedRectangle(cornerRadius: 6)
                            .stroke(option.recommended == true ? Color.accentColor.opacity(0.55) : Color.clear, lineWidth: 1))
                    .cornerRadius(6)
                }
                .buttonStyle(.plain)
                .disabled(vm.isSubmitting)
            }
        }
    }

    private var freeTextControls: some View {
        HStack(spacing: 8) {
            TextField("Your answer", text: $answerText)
                .textFieldStyle(.roundedBorder)
                .disabled(vm.isSubmitting)
            Button("Send") { vm.persistAnswer(answerText) }
                .disabled(vm.isSubmitting || answerText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
    }

    private var campaignApprovalControls: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                Button("Approve") { vm.persistAnswer("approve") }
                    .buttonStyle(.borderedProminent)
                    .disabled(vm.isSubmitting)
                Button("Reject") { vm.persistAnswer("reject") }
                    .buttonStyle(.bordered)
                    .disabled(vm.isSubmitting)
            }
            DisclosureGroup("Amend (advanced)") {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Send an amendment JSON value only when the campaign workflow requires it.")
                        .font(.caption)
                        .foregroundColor(.secondary)
                    TextEditor(text: $amendText)
                        .font(.system(size: 11, design: .monospaced))
                        .frame(minHeight: 90)
                        .overlay(RoundedRectangle(cornerRadius: 4).stroke(Color.gray.opacity(0.25)))
                        .disabled(vm.isSubmitting)
                    HStack {
                        Spacer()
                        Button("Send amendment") { vm.persistAnswer(amendText) }
                            .disabled(vm.isSubmitting || amendText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                }
                .padding(.top, 4)
            }
        }
    }

    private func dismissalControls(for ask: AskRequest) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Dismiss without an answer").font(.caption).foregroundColor(.secondary)
            HStack(spacing: 8) {
                TextField("What was decided (optional)", text: $resolutionText)
                    .textFieldStyle(.roundedBorder)
                    .disabled(vm.isSubmitting || ask.askerSessionID == nil)
                Button("Dismiss") {
                    let resolution = resolutionText.trimmingCharacters(in: .whitespacesAndNewlines)
                    vm.dismiss(resolution: resolution.isEmpty ? nil : resolutionText)
                }
                .buttonStyle(.bordered)
                .disabled(vm.isSubmitting || ask.askerSessionID == nil)
            }
            if ask.askerSessionID == nil {
                Text("Dismiss is unavailable because this record has no asker session.")
                    .font(.caption2)
                    .foregroundColor(.secondary)
            }
        }
    }

    // MARK: Detail helpers

    private func infoSection(_ label: String, _ text: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(label).font(.caption).bold()
            Text(text)
                .font(.system(size: 12))
                .textSelection(.enabled)
        }
    }

    private func refsSection(_ refs: [String]) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Refs").font(.caption).bold()
            ForEach(Array(refs.enumerated()), id: \.offset) { _, ref in
                Text(ref)
                    .font(.system(size: 11, design: .monospaced))
                    .textSelection(.enabled)
            }
        }
    }

    private func reversibilitySection(_ value: Double) -> some View {
        let normalized = min(max(value, 0), 1)
        return VStack(alignment: .leading, spacing: 4) {
            Text("reversibility \(String(format: "%.1f", value))")
                .font(.caption)
                .foregroundColor(.secondary)
            ProgressView(value: normalized)
                .tint(Color.accentColor.opacity(0.65))
        }
    }

    private func detailLine(_ label: String, _ value: String, monospaced: Bool = false) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text("\(label):").font(.caption).foregroundColor(.secondary)
            Text(value)
                .font(monospaced ? .system(size: 11, design: .monospaced) : .system(size: 11))
                .textSelection(.enabled)
        }
    }

    private func shortSession(_ sessionID: String?) -> String {
        guard let sessionID, !sessionID.isEmpty else { return "unknown asker" }
        return sessionID.count > 6 ? String(sessionID.suffix(6)) : sessionID
    }

    private func vetoCountdown(for ask: AskRequest) -> String? {
        guard ask.silencePolicy?.mode == "veto_window",
              let waitUntil = ask.silencePolicy?.waitUntil else { return nil }
        let remaining = Int(Date(timeIntervalSince1970: TimeInterval(waitUntil) / 1_000).timeIntervalSinceNow)
        guard remaining > 0 else { return nil }
        if remaining < 60 { return "auto-proceeds in \(remaining)s" }
        if remaining < 3_600 { return "auto-proceeds in \(Int(ceil(Double(remaining) / 60)))m" }
        return "auto-proceeds in \(Int(ceil(Double(remaining) / 3_600)))h"
    }

    private func vetoDeadlineText(for ask: AskRequest) -> String? {
        guard ask.silencePolicy?.mode == "veto_window",
              let waitUntil = ask.silencePolicy?.waitUntil else { return nil }
        let deadline = Date(timeIntervalSince1970: TimeInterval(waitUntil) / 1_000)
        if deadline <= Date() { return "may have auto-proceeded" }
        return "auto-proceeds at \(absoluteTime(waitUntil))"
    }

    private func relativeTime(_ epochMs: Int64) -> String {
        let interval = Date().timeIntervalSince1970 - Double(epochMs) / 1_000
        let seconds = max(0, Int(interval))
        if seconds < 60 { return "\(seconds)s ago" }
        if seconds < 3_600 { return "\(seconds / 60)m ago" }
        if seconds < 86_400 { return "\(seconds / 3_600)h ago" }
        return "\(seconds / 86_400)d ago"
    }

    private func absoluteTime(_ epochMs: Int64) -> String {
        let formatter = DateFormatter()
        formatter.dateStyle = .medium
        formatter.timeStyle = .medium
        return formatter.string(from: Date(timeIntervalSince1970: TimeInterval(epochMs) / 1_000))
    }
}
