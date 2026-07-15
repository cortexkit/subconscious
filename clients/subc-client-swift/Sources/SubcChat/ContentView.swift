import SwiftUI

struct ContentView: View {
    @StateObject private var vm = ChatViewModel()
    @StateObject private var roomsVM = RoomsViewModel()
    @StateObject private var asksVM = AskViewModel()
    @StateObject private var observeVM = ObserveViewModel()

    var body: some View {
        TabView {
            chatTab
                .tabItem { Label("Chat", systemImage: "bubble.left.and.bubble.right") }
            RoomsView(vm: roomsVM)
                .tabItem { Label("Rooms", systemImage: "person.3") }
            AskView(vm: asksVM)
                .tabItem { Label(asksVM.tabTitle, systemImage: "questionmark.circle") }
            ObserveView(vm: observeVM, lane: .athena)
                .tabItem { Label("Athena", systemImage: "person.2.wave.2") }
            ObserveView(vm: observeVM, lane: .gather)
                .tabItem { Label("Gathers", systemImage: "doc.text.magnifyingglass") }
            ObserveView(vm: observeVM, lane: .checks)
                .tabItem { Label("Checks", systemImage: "checkmark.seal") }
        }
        .frame(minWidth: 760, minHeight: 480)
    }

    private var chatTab: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            VStack(spacing: 0) {
                header
                Divider()
                transcript
                Divider()
                composer
            }
        }
    }

    // MARK: - Sidebar (session picker)

    private var sidebar: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Sessions").font(.headline)
                Spacer()
                Button(action: vm.newSession) {
                    Image(systemName: "square.and.pencil")
                }
                .buttonStyle(.borderless)
                .disabled(vm.isRunning)
                .help("New chat")
            }
            .padding(10)
            Divider()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(vm.sessions) { session in
                        sessionRow(session)
                    }
                }
                .padding(6)
            }
        }
        .frame(width: 210)
        .background(Color(NSColor.controlBackgroundColor))
    }

    private func sessionRow(_ session: ChatSession) -> some View {
        let isActive = session.id == vm.activeId
        return HStack {
            VStack(alignment: .leading, spacing: 1) {
                Text(session.title.isEmpty ? "New chat" : session.title)
                    .lineLimit(1)
                    .font(.system(size: 12, weight: isActive ? .semibold : .regular))
                Text(session.messages.isEmpty ? "empty" : "\(session.messages.count) messages")
                    .font(.system(size: 10))
                    .foregroundColor(.secondary)
            }
            Spacer()
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(isActive ? Color.accentColor.opacity(0.18) : Color.clear)
        .cornerRadius(6)
        .contentShape(Rectangle())
        .onTapGesture { vm.selectSession(session.id) }
        .contextMenu {
            Button("Delete", role: .destructive) { vm.deleteSession(session.id) }
                .disabled(vm.isRunning)
        }
    }

    // MARK: - Header (model picker + status)

    private var header: some View {
        HStack(spacing: 8) {
            Text("CortexKit Chat").font(.headline)
            Text("broca over subc").font(.caption).foregroundColor(.secondary)
            Spacer()
            // Model picker: known-good presets, plus a free-text field for any catalog model.
            Menu {
                ForEach(MODEL_PRESETS, id: \.self) { preset in
                    Button(preset) { vm.model = preset }
                }
            } label: {
                Image(systemName: "cpu")
            }
            .menuStyle(.borderlessButton)
            .frame(width: 24)
            .help("Pick a known-good model")
            TextField("provider/model", text: $vm.model)
                .textFieldStyle(.roundedBorder)
                .frame(width: 220)
            Toggle(isOn: $vm.toolsEnabled) {
                Image(systemName: "wrench.and.screwdriver")
            }
            .toggleStyle(.button)
            .help("Give the model aft's tools (read/edit/grep/… against this chat's project folder)")
            Button {
                vm.pickProjectRoot()
            } label: {
                HStack(spacing: 3) {
                    Image(systemName: "folder")
                    Text((vm.activeProjectRoot as NSString).lastPathComponent)
                        .font(.caption)
                        .lineLimit(1)
                        .frame(maxWidth: 110)
                }
            }
            .buttonStyle(.borderless)
            .disabled(!vm.canPickProjectRoot)
            .help(vm.canPickProjectRoot
                ? "Choose the project folder tools operate on (fixed after the first message): \(vm.activeProjectRoot)"
                : "Project folder (fixed once the session has messages): \(vm.activeProjectRoot)")
            Circle()
                .fill(vm.status == "error" ? Color.red : (vm.isRunning ? Color.orange : Color.green))
                .frame(width: 8, height: 8)
            Text(vm.status).font(.caption).foregroundColor(.secondary).frame(width: 110, alignment: .leading)
        }
        .padding(10)
    }

    // MARK: - Transcript

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    if let idx = vm.activeIndex {
                        ForEach(vm.sessions[idx].messages) { msg in
                            bubble(msg).id(msg.id)
                        }
                    }
                }
                .padding(12)
            }
            .onChange(of: vm.activeIndex.map { vm.sessions[$0].messages.last?.text ?? "" } ?? "") { _ in
                if let idx = vm.activeIndex, let last = vm.sessions[idx].messages.last {
                    proxy.scrollTo(last.id, anchor: .bottom)
                }
            }
        }
    }

    private func bubble(_ msg: ChatMessage) -> some View {
        HStack {
            if msg.role == .user { Spacer(minLength: 40) }
            VStack(alignment: .leading, spacing: 2) {
                Text(headerLabel(msg)).font(.caption2).foregroundColor(.secondary)
                Text(msg.pending && msg.text.isEmpty ? "…" : msg.text)
                    .textSelection(.enabled)
                    .padding(10)
                    .background(background(msg))
                    .cornerRadius(10)
            }
            if msg.role != .user { Spacer(minLength: 40) }
        }
    }

    private func headerLabel(_ msg: ChatMessage) -> String {
        switch msg.role {
        case .user: return "you"
        case .assistant:
            if msg.isError { return "error" }
            return msg.model.map { "assistant · \($0)" } ?? "assistant"
        case .system: return "system"
        }
    }

    private func background(_ msg: ChatMessage) -> Color {
        if msg.isError { return Color.red.opacity(0.14) }
        switch msg.role {
        case .user: return Color.accentColor.opacity(0.18)
        case .assistant: return Color.gray.opacity(0.14)
        case .system: return Color.yellow.opacity(0.12)
        }
    }

    // MARK: - Composer

    private var composer: some View {
        HStack(spacing: 8) {
            // Same single-trigger rule as the rooms composer: Return submits via
            // onSubmit only (dual onCommit+shortcut paths double-fire; here it was
            // masked by isRunning, but the structure was the same bug).
            TextField("Message…", text: $vm.input)
                .textFieldStyle(.roundedBorder)
                .disabled(vm.isRunning)
                .onSubmit(vm.send)
            Button("Send", action: vm.send)
                .disabled(vm.isRunning || vm.input.trimmingCharacters(in: .whitespaces).isEmpty)
        }
        .padding(10)
    }
}
