import SwiftUI

struct ContentView: View {
    @StateObject private var vm = ChatViewModel()

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            transcript
            Divider()
            composer
        }
        .frame(minWidth: 560, minHeight: 460)
    }

    private var header: some View {
        HStack(spacing: 8) {
            Text("CortexKit Chat").font(.headline)
            Text("llm-runner over subc").font(.caption).foregroundColor(.secondary)
            Spacer()
            TextField("provider/model", text: $vm.model)
                .textFieldStyle(.roundedBorder)
                .frame(width: 220)
            Circle()
                .fill(vm.isRunning ? Color.orange : Color.green)
                .frame(width: 8, height: 8)
            Text(vm.status).font(.caption).foregroundColor(.secondary).frame(width: 90, alignment: .leading)
        }
        .padding(10)
    }

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(vm.messages) { msg in
                        bubble(msg).id(msg.id)
                    }
                }
                .padding(12)
            }
            .onChange(of: vm.messages.count) { _ in
                if let last = vm.messages.last { proxy.scrollTo(last.id, anchor: .bottom) }
            }
        }
    }

    private func bubble(_ msg: ChatMessage) -> some View {
        HStack {
            if msg.role == .user { Spacer(minLength: 40) }
            VStack(alignment: .leading, spacing: 2) {
                Text(label(msg.role)).font(.caption2).foregroundColor(.secondary)
                Text(msg.pending && msg.text.isEmpty ? "…" : msg.text)
                    .textSelection(.enabled)
                    .padding(10)
                    .background(background(msg.role))
                    .cornerRadius(10)
            }
            if msg.role != .user { Spacer(minLength: 40) }
        }
    }

    private func label(_ role: ChatMessage.Role) -> String {
        switch role {
        case .user: return "you"
        case .assistant: return "assistant"
        case .system: return "system"
        }
    }

    private func background(_ role: ChatMessage.Role) -> Color {
        switch role {
        case .user: return Color.accentColor.opacity(0.18)
        case .assistant: return Color.gray.opacity(0.14)
        case .system: return Color.yellow.opacity(0.12)
        }
    }

    private var composer: some View {
        HStack(spacing: 8) {
            TextField("Message…", text: $vm.input, onCommit: vm.send)
                .textFieldStyle(.roundedBorder)
                .disabled(vm.isRunning)
            Button("Send", action: vm.send)
                .keyboardShortcut(.return, modifiers: [])
                .disabled(vm.isRunning || vm.input.trimmingCharacters(in: .whitespaces).isEmpty)
        }
        .padding(10)
    }
}
