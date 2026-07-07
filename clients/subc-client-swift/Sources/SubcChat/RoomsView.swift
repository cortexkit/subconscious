import SwiftUI

/// The Rooms tab: a human seat in multi-agent meetings. Left rail lists rooms
/// (state chip, unread badge, invite accept/decline); the main pane shows the
/// transcript, a board strip, and the composer with the signal vocabulary.
struct RoomsView: View {
    @ObservedObject var vm: RoomsViewModel

    var body: some View {
        HStack(spacing: 0) {
            roomsSidebar
            Divider()
            if vm.activeRoomId != nil {
                roomPane
            } else {
                placeholder
            }
        }
        .onAppear { vm.appear() }
        .onDisappear { vm.disappear() }
    }

    // MARK: - Sidebar

    private var roomsSidebar: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Rooms").font(.headline)
                Spacer()
                Circle()
                    .fill(vm.connected ? Color.green : Color.gray)
                    .frame(width: 8, height: 8)
                    .help(vm.connected ? "connected" : "not connected")
            }
            .padding(10)
            Divider()
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(vm.rows) { row in
                        roomRow(row)
                    }
                    if vm.rows.isEmpty {
                        Text("No rooms yet.\nYou get invited by session id:")
                            .font(.system(size: 10))
                            .foregroundColor(.secondary)
                            .padding(.top, 12)
                            .frame(maxWidth: .infinity)
                    }
                }
                .padding(6)
            }
            Divider()
            identityFooter
        }
        .frame(width: 230)
        .background(Color(NSColor.controlBackgroundColor))
    }

    private func roomRow(_ row: RoomsListRow) -> some View {
        let isActive = row.room.roomId == vm.activeRoomId
        let unread = row.unreadCount ?? 0
        return VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(row.room.title ?? row.room.roomId)
                    .lineLimit(1)
                    .font(.system(size: 12, weight: isActive ? .semibold : .regular))
                Spacer()
                if unread > 0 {
                    Text("\(unread)")
                        .font(.system(size: 9, weight: .bold))
                        .padding(.horizontal, 5).padding(.vertical, 1)
                        .background(Color.accentColor)
                        .foregroundColor(.white)
                        .clipShape(Capsule())
                }
            }
            HStack(spacing: 4) {
                stateChip(row.room.state)
                if row.pendingInvite == true {
                    Button("Accept") { vm.rsvp(row.room.roomId, accept: true) }
                        .buttonStyle(.borderedProminent).controlSize(.mini)
                    Button("Decline") { vm.rsvp(row.room.roomId, accept: false) }
                        .buttonStyle(.bordered).controlSize(.mini)
                }
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 6)
        .background(isActive ? Color.accentColor.opacity(0.18) : Color.clear)
        .cornerRadius(6)
        .contentShape(Rectangle())
        .onTapGesture { vm.selectRoom(row.room.roomId) }
    }

    private func stateChip(_ state: String) -> some View {
        Text(state)
            .font(.system(size: 9, weight: .medium))
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(stateColor(state).opacity(0.2))
            .foregroundColor(stateColor(state))
            .clipShape(Capsule())
    }

    private func stateColor(_ state: String) -> Color {
        switch state {
        case "active": return .green
        case "starting", "convened": return .orange
        case "adjourned": return .secondary
        case "cancelled": return .red
        default: return .secondary
        }
    }

    /// The app's room identity, shown so a chair knows what to invite.
    private var identityFooter: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("your identity").font(.system(size: 9)).foregroundColor(.secondary)
            Text(vm.sessionId)
                .font(.system(size: 9, design: .monospaced))
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .padding(8)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - Room pane

    private var placeholder: some View {
        VStack {
            Spacer()
            Text("Select a room").foregroundColor(.secondary)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private var roomPane: some View {
        VStack(spacing: 0) {
            roomHeader
            Divider()
            boardStrip
            Divider()
            transcript
            Divider()
            roomComposer
        }
    }

    private var roomHeader: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(vm.snapshot?.room.title ?? vm.activeRoomId ?? "")
                    .font(.headline).lineLimit(1)
                if let snap = vm.snapshot {
                    Text("\(snap.members.count) members · head #\(snap.headSeq)")
                        .font(.caption2).foregroundColor(.secondary)
                }
            }
            Spacer()
            if let state = vm.snapshot?.room.state { stateChip(state) }
            Button("Enter") { vm.enter() }.controlSize(.small)
            Button("Leave") { vm.leave() }.controlSize(.small)
            Text(vm.status)
                .font(.caption2).foregroundColor(.secondary)
                .lineLimit(1).frame(maxWidth: 200, alignment: .trailing)
        }
        .padding(10)
    }

    /// One cell per member: display name, reaction, raised hand, stage holder.
    private var boardStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 10) {
                ForEach(vm.snapshot?.members ?? [], id: \.identity.sessionId) { member in
                    boardCell(member)
                }
            }
            .padding(.horizontal, 10).padding(.vertical, 6)
        }
        .frame(height: 44)
    }

    private func boardCell(_ member: RoomMember) -> some View {
        let cell = vm.snapshot?.board?.first { $0.identity == member.identity }?.cell
        let holdsStage = vm.snapshot?.stage?.holder == member.identity
        return HStack(spacing: 4) {
            if holdsStage { Image(systemName: "mic.fill").font(.system(size: 9)).foregroundColor(.green) }
            Text(vm.displayName(for: member.identity)).font(.system(size: 11, weight: .medium))
            if let reaction = cell?.reaction { Text(reactionGlyph(reaction.kind)).font(.system(size: 11)) }
            if cell?.floorRequest == true { Text("✋").font(.system(size: 11)) }
        }
        .padding(.horizontal, 8).padding(.vertical, 4)
        .background(holdsStage ? Color.green.opacity(0.12) : Color.gray.opacity(0.10))
        .cornerRadius(8)
    }

    private func reactionGlyph(_ kind: String) -> String {
        switch kind {
        case "ACK": return "✓"
        case "ACK_AGREE": return "👍"
        case "ACK_DISAGREE": return "👎"
        case "ACK_ABSTAIN": return "➖"
        default: return kind
        }
    }

    // MARK: - Transcript

    private var transcript: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 8) {
                    ForEach(vm.events) { event in
                        eventView(event).id(event.seq)
                    }
                }
                .padding(12)
            }
            .onChange(of: vm.events.last?.seq ?? 0) { _ in
                if let last = vm.events.last {
                    proxy.scrollTo(last.seq, anchor: .bottom)
                }
            }
        }
    }

    @ViewBuilder
    private func eventView(_ event: RoomEvent) -> some View {
        switch event.kind {
        case "post":
            postBubble(event)
        case "signal":
            caption("\(vm.authorLabel(event.author)) · \(reactionGlyph(event.body?.kind ?? "signal"))"
                + (event.body?.note.map { " — \($0)" } ?? ""))
        case "cancelled":
            caption("meeting cancelled" + (event.body?.reason.map { ": \($0)" } ?? ""))
        default:
            // enter/leave/rsvp/starting/meeting_started/stage_*/agenda_advance/adjourn
            caption("\(vm.authorLabel(event.author)) · \(event.kind.replacingOccurrences(of: "_", with: " "))")
        }
    }

    private func postBubble(_ event: RoomEvent) -> some View {
        let mine = vm.isSelf(event.author)
        return HStack {
            if mine { Spacer(minLength: 40) }
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(vm.authorLabel(event.author)).font(.caption2).foregroundColor(.secondary)
                    Text("#\(event.seq)").font(.system(size: 8)).foregroundColor(.secondary.opacity(0.6))
                    if let reply = event.body?.replyToSeq {
                        Text("↩︎ #\(reply)").font(.system(size: 8)).foregroundColor(.secondary.opacity(0.6))
                    }
                }
                Text(event.body?.text ?? "")
                    .textSelection(.enabled)
                    .padding(10)
                    .background(mine ? Color.accentColor.opacity(0.18) : Color.gray.opacity(0.14))
                    .cornerRadius(10)
            }
            if !mine { Spacer(minLength: 40) }
        }
    }

    private func caption(_ text: String) -> some View {
        Text(text)
            .font(.caption2)
            .foregroundColor(.secondary)
            .frame(maxWidth: .infinity, alignment: .center)
    }

    // MARK: - Composer

    private var roomComposer: some View {
        VStack(spacing: 6) {
            // Signals are real wire commands (rooms.signal) that land on every
            // member's board; optional for humans but labeled so that's clear.
            HStack(spacing: 6) {
                signalButton("ACK", "✓ Ack")
                signalButton("ACK_AGREE", "👍 Agree")
                signalButton("ACK_DISAGREE", "👎 Disagree")
                signalButton("ACK_ABSTAIN", "➖ Abstain")
                signalButton("REQUEST_STAGE", "✋ Raise hand")
                signalButton("RAISE_WITHDRAW", "🤚 Withdraw")
                Spacer()
            }
            HStack(spacing: 8) {
                // Return submits via onSubmit ONLY; the button is click-only. Giving the
                // button a .return keyboardShortcut too made one keypress fire both paths
                // (onCommit + shortcut), and the field's end-of-editing binding writeback
                // restored the cleared text between them — every Return double-posted.
                TextField("Message the room…", text: $vm.composer)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(vm.post)
                Button("Post", action: vm.post)
                    .disabled(vm.composer.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(10)
    }

    private func signalButton(_ kind: String, _ glyph: String) -> some View {
        Button(glyph) { vm.signal(kind) }
            .buttonStyle(.bordered)
            .controlSize(.small)
            .help(kind)
    }
}
