import SwiftUI

private enum DashboardMode: String {
    case attention, browse
}

/// A cross-computer inbox alongside the complete computer inventory.
struct DashboardView: View {
    @Environment(AppModel.self) private var model
    @State private var form: ComputerFormTarget?
    @State private var setAsideError: String?
    @State private var newSessionComputer: Computer?
    @AppStorage("dashboard.mode.v1") private var mode = DashboardMode.attention
    @State private var errorsExpanded = false
    @State private var showingReportingSetup = false

    var body: some View {
        @Bindable var model = model
        NavigationStack(path: $model.path) {
            content
                .navigationTitle(title)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Button {
                            form = .new
                        } label: {
                            Label("Add Computer", systemImage: "plus")
                        }
                        .accessibilityIdentifier(AccessibilityID.addComputer)
                        .disabled(model.storageError != nil)
                    }
                }
                .navigationDestination(for: SessionRoute.self) { route in
                    if let controller = model.terminalController(for: route) {
                        TerminalScreen(controller: controller)
                    } else {
                        ContentUnavailableView("Computer removed", systemImage: "desktopcomputer.trianglebadge.exclamationmark")
                    }
                }
        }
        .onChange(of: model.attention.showsErrorsByDefault, initial: true) { _, showErrors in
            if showErrors { errorsExpanded = true }
        }
        .sheet(item: $form) { target in
            ComputerFormView(target: target)
        }
        .sheet(item: $newSessionComputer) { computer in
            NewSessionView(computer: computer)
        }
        .sheet(isPresented: $showingReportingSetup) {
            ReportingSetupView()
        }
        .sheet(item: $model.hostKeyPrompt) { prompt in
            HostKeyPromptView(prompt: prompt)
        }
        .alert("End Session?", isPresented: Binding(
            get: { model.endSessionPrompt != nil },
            set: { if !$0 { model.endSessionPrompt = nil } }
        ), presenting: model.endSessionPrompt) { prompt in
            Button("Cancel", role: .cancel) {
                model.endSessionPrompt = nil
            }
            .keyboardShortcut(.defaultAction)
            .accessibilityIdentifier(AccessibilityID.endSessionCancel)
            Button("End Session", role: .destructive) {
                model.confirmEndSession(prompt)
            }
            .accessibilityIdentifier(AccessibilityID.endSessionConfirm)
        } message: { prompt in
            Text(prompt.warning)
        }
        .alert("Session End Not Confirmed", isPresented: Binding(
            get: { model.endSessionError != nil },
            set: { if !$0 { model.endSessionError = nil } }
        )) {
            Button("OK", role: .cancel) { model.endSessionError = nil }
        } message: {
            Text(model.endSessionError ?? "")
                .accessibilityIdentifier(AccessibilityID.endSessionError)
        }
        .safeAreaInset(edge: .bottom) {
            if let prompt = model.endingSession {
                HStack {
                    ProgressView()
                    Text("Ending \(prompt.description)…")
                        .font(.footnote)
                }
                .padding()
                .frame(maxWidth: .infinity)
                .background(.regularMaterial)
                .accessibilityIdentifier(AccessibilityID.endSessionProgress)
            }
        }
    }

    private var title: String { "Delux" }

    @ViewBuilder
    private var content: some View {
        if let storageError = model.storageError {
            ContentUnavailableView {
                Label("Saved computers unreadable", systemImage: "externaldrive.badge.exclamationmark")
            } description: {
                Text(storageError.localizedDescription)
                    .accessibilityIdentifier(AccessibilityID.storageError)
            } actions: {
                Button("Set File Aside and Start Over") {
                    do {
                        try model.setAsideUnreadableStorage()
                    } catch {
                        setAsideError = error.localizedDescription
                    }
                }
                .buttonStyle(.bordered)
                .accessibilityIdentifier(AccessibilityID.storageSetAside)
                if let setAsideError {
                    Text(setAsideError)
                        .font(.caption)
                        .foregroundStyle(.red)
                }
            }
            .accessibilityIdentifier(AccessibilityID.dashboard)
        } else if model.computers.isEmpty {
            ContentUnavailableView {
                Label("No computers yet", systemImage: "desktopcomputer")
            } description: {
                Text("Add a computer you can reach over SSH. Tailscale and other VPN addresses work when that VPN app is connected.")
            } actions: {
                Button("Add Computer") { form = .new }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier(AccessibilityID.emptyDashboard)
            }
            .accessibilityIdentifier(AccessibilityID.dashboard)
        } else {
            List {
                Section {
                    Picker("Dashboard view", selection: $mode) {
                        Text("Attention · \(model.waitingCount)").tag(DashboardMode.attention)
                            .accessibilityIdentifier(AccessibilityID.attentionMode)
                        Text("Browse").tag(DashboardMode.browse)
                            .accessibilityIdentifier(AccessibilityID.browseMode)
                    }
                    .pickerStyle(.segmented)
                    .accessibilityIdentifier(AccessibilityID.dashboardMode)
                    .listRowBackground(Color.clear)
                }
                if !model.isOnline {
                    Section {
                        Label("Offline — sessions will refresh when the network returns.", systemImage: "wifi.slash")
                            .foregroundStyle(.secondary)
                            .accessibilityIdentifier(AccessibilityID.offlineBanner)
                    }
                }
                if mode == .attention {
                    attentionSections
                } else {
                    ForEach(model.computers) { computer in
                        ComputerSection(
                            computer: computer,
                            state: model.state(computer.id),
                            isOnline: model.isOnline,
                            onEdit: { form = .edit(computer) },
                            onNewSession: { newSessionComputer = computer }
                        )
                    }
                }
            }
            .listStyle(.insetGrouped)
            .refreshable {
                await withTaskGroup(of: Void.self) { group in
                    for computer in model.computers {
                        group.addTask { await model.refreshAndWait(computer.id) }
                    }
                }
            }
            .accessibilityIdentifier(AccessibilityID.dashboard)
        }
    }

    @ViewBuilder
    private var attentionSections: some View {
        Section {
            if model.attention.waiting.isEmpty {
                VStack(alignment: .leading, spacing: Spacing.xs) {
                    Text("No reported waiting sessions")
                    if !model.attention.isComplete {
                        Text("Coverage is incomplete. Check the computers and unreported sessions below.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            } else {
                ForEach(model.attention.waiting) { entry in
                    InboxSessionRow(entry: entry)
                }
            }
        } header: {
            Text("Needs input · \(model.waitingCount)")
                .font(.subheadline)
                .fontWeight(.regular)
                .foregroundStyle(.secondary)
                .textCase(nil)
                .accessibilityIdentifier(AccessibilityID.attentionCount)
        }
        if !model.attention.errors.isEmpty {
            Section {
                DisclosureGroup(isExpanded: $errorsExpanded) {
                    ForEach(model.attention.errors) { entry in
                        InboxSessionRow(entry: entry)
                    }
                } label: {
                    Text("Errors · \(model.attention.errors.count)")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .accessibilityIdentifier(AccessibilityID.attentionErrors)
                }
            }
        }
        Section {
            DisclosureGroup {
                ForEach(model.attention.coverage) { coverage in
                    if coverage.availability != .checked || coverage.unreportedCount > 0 {
                        VStack(alignment: .leading, spacing: Spacing.xs) {
                            Text(coverage.computer.name)
                            Text(coverageDetail(coverage))
                                .foregroundStyle(.secondary)
                        }
                        .font(.caption)
                        .accessibilityIdentifier(AccessibilityID.attentionCoverage + "." + coverage.id.uuidString)
                    }
                }
                Button("Browse all computers and sessions") { mode = .browse }
                    .accessibilityIdentifier(AccessibilityID.attentionCoverage + ".browse")
            } label: {
                VStack(alignment: .leading, spacing: Spacing.xs) {
                    let coverage = model.attention.coverage
                    Text("\(coverage.filter { $0.availability == .checked }.count) checked · \(coverage.filter { $0.availability == .checking }.count) checking · \(coverage.filter { $0.availability == .unavailable }.count) unavailable")
                    if model.attention.unreportedCount > 0 {
                        Text("\(model.attention.unreportedCount) sessions without reported status")
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                .accessibilityElement(children: .combine)
                .accessibilityIdentifier(AccessibilityID.attentionCoverage + ".details")
            }
            Button("Set up reporting") { showingReportingSetup = true }
                .font(.caption)
                .accessibilityIdentifier(AccessibilityID.reportingSetup)
        } header: {
            Text("Coverage")
                .font(.subheadline)
                .fontWeight(.regular)
                .foregroundStyle(.secondary)
                .textCase(nil)
        }
    }

    private func coverageDetail(_ coverage: ComputerCoverage) -> String {
        switch coverage.availability {
        case .checking: return "Checking"
        case .unavailable: return "Unavailable"
        case .checked: return "\(coverage.unreportedCount) sessions without reported status"
        }
    }
}

enum ComputerFormTarget: Identifiable {
    case new
    case edit(Computer)

    var id: UUID? {
        switch self {
        case .new: return nil
        case .edit(let computer): return computer.id
        }
    }
}

private struct InboxSessionRow: View {
    @Environment(AppModel.self) private var model
    let entry: InboxSession

    var body: some View {
        Button {
            model.open(entry.session, on: entry.computer)
        } label: {
            HStack(spacing: Spacing.md) {
                Image(systemName: StatusStyle.symbol(entry.session.status))
                    .font(.caption)
                    .foregroundStyle(StatusStyle.color(entry.session.status))
                    .accessibilityHidden(true)
                VStack(alignment: .leading, spacing: Spacing.xs) {
                    Text(entry.session.name)
                        .font(.body.monospaced())
                        .lineLimit(1)
                    Text("\(entry.computer.name) · \(entry.computer.hostname)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer(minLength: 0)
                Image(systemName: "chevron.right")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                    .accessibilityHidden(true)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(entry.session.name), \(StatusStyle.label(entry.session.status)), \(entry.computer.name), \(entry.computer.hostname)")
        .accessibilityIdentifier(AccessibilityID.attentionRow(entry.id))
        .contextMenu {
            Button(role: .destructive) {
                model.requestEndSession(entry.session, on: entry.computer)
            } label: {
                Label("End Session…", systemImage: "stop.circle")
            }
            .disabled(!model.isOnline || model.endingSession != nil)
        }
    }
}

private struct ComputerSection: View {
    @Environment(AppModel.self) private var model
    let computer: Computer
    let state: ComputerState
    let isOnline: Bool
    let onEdit: () -> Void
    let onNewSession: () -> Void

    var body: some View {
        Section {
            rows
        } header: {
            header
                .accessibilityIdentifier(AccessibilityID.computerSection(computer.id))
        }
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline, spacing: Spacing.sm) {
            VStack(alignment: .leading, spacing: Spacing.xs) {
                Text(computer.name)
                    .font(.subheadline)
                    .fontWeight(.regular)
                    .textCase(nil)
                    .foregroundStyle(.secondary)
                let address = state.connectedAddress ?? SSHAddress(hostname: computer.hostname, port: computer.port)
                Text("\(computer.username)@\(address.hostname):\(String(address.port))\(address == computer.lanAddress ? " · LAN" : "")")
                    .font(.caption.monospaced())
                    .textCase(nil)
            }
            Spacer()
            statusCaption
            Menu {
                Button("New Session", systemImage: "plus.rectangle.on.rectangle", action: onNewSession)
                    .accessibilityIdentifier(AccessibilityID.computerNewSession(computer.id))
                Button("Refresh", systemImage: "arrow.clockwise") { model.refresh(computer.id) }
                    .accessibilityIdentifier(AccessibilityID.computerRefresh(computer.id))
                Button("Edit Computer", systemImage: "pencil", action: onEdit)
                    .accessibilityIdentifier(AccessibilityID.computerEdit(computer.id))
            } label: {
                Image(systemName: "ellipsis.circle")
                    .font(.body)
                    .accessibilityLabel("\(computer.name) actions")
            }
            .accessibilityIdentifier(AccessibilityID.computerSection(computer.id) + ".menu")
        }
        .padding(.bottom, Spacing.xs)
    }

    @ViewBuilder
    private var statusCaption: some View {
        if state.isRefreshing, state.hasLoadedOnce {
            ProgressView()
                .controlSize(.mini)
                .accessibilityLabel("Refreshing")
        } else if let date = state.lastRefresh, case .loaded = state.phase {
            Text(date, style: .relative)
                .font(.caption2)
                .textCase(nil)
                .accessibilityLabel("Updated \(date.formatted(.relative(presentation: .named))) ago")
        }
    }

    @ViewBuilder
    private var rows: some View {
        if !isOnline {
            StatusRow(symbol: "wifi.slash", title: "Offline", detail: "Waiting for a fresh inventory.", tint: .secondary)
                .accessibilityIdentifier(AccessibilityID.computerStatus(computer.id))
            staleSessions
        } else {
        switch state.phase {
        case .idle, .loading:
            StatusRow(symbol: nil, title: "Connecting…", detail: nil, tint: .secondary, progress: true)
                .accessibilityIdentifier(AccessibilityID.computerStatus(computer.id))
            staleSessions
        case .loaded:
            if state.sessions.isEmpty {
                VStack(alignment: .leading, spacing: Spacing.sm) {
                    Text("No tmux sessions")
                        .foregroundStyle(.secondary)
                    Button("New Session", action: onNewSession)
                        .buttonStyle(.bordered)
                        .accessibilityIdentifier(AccessibilityID.computerNewSession(computer.id) + ".empty")
                }
                .padding(.vertical, Spacing.xs)
                .accessibilityIdentifier(AccessibilityID.computerStatus(computer.id))
            } else {
                sessionRows
            }
        case .failed(let message):
            StatusRow(symbol: "exclamationmark.triangle", title: "Could not load sessions", detail: message, tint: .red) {
                Button("Retry") { model.refresh(computer.id) }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier(AccessibilityID.computerRefresh(computer.id) + ".retry")
            }
            .accessibilityIdentifier(AccessibilityID.computerStatus(computer.id))
            staleSessions
        case .hostKeyUnknown:
            StatusRow(symbol: "key", title: "Host key not verified", detail: "Confirm this computer's SSH fingerprint before Delux signs in.", tint: .orange) {
                Button("Verify…") { model.presentHostKeyPrompt(for: computer) }
                    .buttonStyle(.borderedProminent)
                    .accessibilityIdentifier(AccessibilityID.computerVerifyHostKey(computer.id))
            }
            .accessibilityIdentifier(AccessibilityID.computerStatus(computer.id))
        case .hostKeyChanged(let expected, let received):
            StatusRow(
                symbol: "lock.slash",
                title: "Host key changed — connection blocked",
                detail: "The key this computer presented is not the one you trusted. Someone may be intercepting the connection. If the server was reinstalled, forget the stored key in Edit Computer and verify the new fingerprint out of band.\nTrusted: \(expected)\nReceived: \(received)",
                tint: .red
            )
            .accessibilityIdentifier(AccessibilityID.computerStatus(computer.id))
        }
        }
    }

    @ViewBuilder
    private var staleSessions: some View {
        if !state.sessions.isEmpty, let date = state.lastRefresh {
            Text("Showing sessions from \(date.formatted(date: .omitted, time: .shortened))")
                .font(.caption)
                .foregroundStyle(.secondary)
            sessionRows
        }
    }

    private var sessionRows: some View {
        ForEach(state.sessions, id: \.lifetime) { session in
            Button {
                model.open(session, on: computer)
            } label: {
                SessionRow(session: session, isCurrent: isOnline && state.phase == .loaded)
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier(AccessibilityID.sessionRow(computer: computer.id, session: session.id) + ".\(session.serverPID).\(session.serverStarted).\(session.created)")
            .contextMenu {
                Button(role: .destructive) {
                    model.requestEndSession(session, on: computer)
                } label: {
                    Label("End Session…", systemImage: "stop.circle")
                }
                .disabled(!model.isOnline || model.endingSession != nil)
                .accessibilityIdentifier(AccessibilityID.sessionEnd(computer: computer.id, session: session.id))
            }
        }
    }
}

private struct SessionRow: View {
    let session: RemoteSession
    var isCurrent = true

    var body: some View {
        HStack(spacing: Spacing.md) {
            Image(systemName: StatusStyle.symbol(session.status))
                .font(.caption.weight(.bold))
                .foregroundStyle(isCurrent ? StatusStyle.color(session.status) : .secondary)
                .frame(width: 16)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: Spacing.xs) {
                Text(session.name)
                    .font(.body.monospaced())
                    .fontWeight(isCurrent && session.status == .waiting ? .semibold : .regular)
                    .lineLimit(1)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(isCurrent && session.status == .waiting ? StatusStyle.color(.waiting) : .secondary)
            }
            Spacer(minLength: 0)
            Image(systemName: "chevron.right")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.tertiary)
                .accessibilityHidden(true)
        }
        .contentShape(Rectangle())
        .padding(.vertical, Spacing.xs)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(session.name), \(detail)")
    }

    private var detail: String {
        let windows = session.windows == 1 ? "1 window" : "\(session.windows) windows"
        let status = StatusStyle.label(session.status)
        return "\(isCurrent ? status : "Last snapshot: " + status) · \(windows)"
    }
}

/// One row describing a computer's connection state, with an optional action.
private struct StatusRow<Action: View>: View {
    let symbol: String?
    let title: String
    let detail: String?
    let tint: Color
    var progress = false
    @ViewBuilder let action: () -> Action

    init(symbol: String?, title: String, detail: String?, tint: Color, progress: Bool = false,
         @ViewBuilder action: @escaping () -> Action) {
        self.symbol = symbol
        self.title = title
        self.detail = detail
        self.tint = tint
        self.progress = progress
        self.action = action
    }

    var body: some View {
        HStack(alignment: .top, spacing: Spacing.md) {
            if progress {
                ProgressView()
                    .controlSize(.small)
                    .frame(width: 16)
            } else if let symbol {
                Image(systemName: symbol)
                    .foregroundStyle(tint)
                    .frame(width: 16)
                    .accessibilityHidden(true)
            }
            VStack(alignment: .leading, spacing: Spacing.sm) {
                Text(title)
                    .foregroundStyle(tint == .secondary ? .secondary : .primary)
                if let detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
                action()
            }
        }
        .padding(.vertical, Spacing.xs)
    }
}

extension StatusRow where Action == EmptyView {
    init(symbol: String?, title: String, detail: String?, tint: Color, progress: Bool = false) {
        self.init(symbol: symbol, title: title, detail: detail, tint: tint, progress: progress) { EmptyView() }
    }
}
